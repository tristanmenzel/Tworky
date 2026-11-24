<script lang="ts">
  import { untrack } from 'svelte'
  import { api, type ConfigConnected, type ConfigSyncRecord } from './api'
  import { ask } from './confirm.svelte'
  import { formatSyncTime } from './configSync'
  import { RemoteConfirmation } from './remoteCheck.svelte'
  import RemoteWarnings from './RemoteWarnings.svelte'

  interface Props {
    visible: boolean
    sync: ConfigSyncRecord | null
    onSync: (record: ConfigSyncRecord) => void
    onError: (message: string) => void
  }

  let { visible, sync, onSync, onError }: Props = $props()

  let remote = $state<string | null>(null)
  let loaded = $state(false)
  let owners = $state<string[] | null>(null)
  let ownersError = $state<string | null>(null)
  let owner = $state('')
  let typed = $state('')
  let busy = $state(false)
  let error = $state<string | null>(null)
  /** What Tworky warned about the remote it just connected. */
  let connectedWarnings = $state<string[]>([])
  const confirmation = new RemoteConfirmation()
  /** For a remote that was added outside Tworky. */
  const adoption = new RemoteConfirmation()

  const ownerIsOrg = $derived(!!owners?.length && owner !== owners[0])
  const unconfirmed = $derived(sync?.state.kind === 'unconfirmed_remote')
  const visibility = $derived(unconfirmed ? 'unknown' : (sync?.visibility ?? 'unknown'))

  $effect(() => {
    void remote
    adoption.reset()
  })

  $effect(() => {
    void typed
    confirmation.reset()
  })

  $effect(() => {
    if (visible) untrack(load)
  })

  async function load() {
    try {
      remote = (await api.configRepoStatus()).remote
      loaded = true
    } catch (e) {
      onError(String(e))
    }
    if (remote === null && owners === null) loadOwners()
  }

  async function loadOwners() {
    try {
      owners = await api.configGithubOwners()
      owner ||= owners[0] ?? ''
    } catch (e) {
      owners = []
      ownersError = `Couldn't list your GitHub accounts: ${e}`
    }
  }

  async function run(action: () => Promise<void>) {
    busy = true
    error = null
    try {
      await action()
    } catch (e) {
      error = String(e)
    } finally {
      busy = false
    }
  }

  async function connected(result: ConfigConnected) {
    connectedWarnings = result.check.warnings
    typed = ''
    onSync(result.sync)
    await load()
  }

  function create() {
    return run(async () => connected(await api.createConfigRemote(owner || null)))
  }

  function useExisting() {
    const target = typed.trim()
    if (!target) return
    return run(async () => {
      const result = await confirmation.connect(
        () => api.checkConfigRemote(target),
        (seen) => api.setConfigRemote(target, seen),
      )
      if (result) await connected(result)
    })
  }

  function adopt() {
    return run(async () => {
      const result = await adoption.connect(
        () => api.checkConfigRemote(null),
        (seen) => api.confirmConfigRemote(seen),
      )
      if (result) await connected(result)
    })
  }

  async function remove() {
    if (!remote) return
    const ok = await ask({
      title: 'Stop syncing your config?',
      message: `Tworky stops syncing with ${remote}. The repo itself stays on GitHub, and your config stays on this machine.`,
      confirmLabel: 'Remove remote',
      danger: true,
    })
    if (!ok) return
    await run(async () => {
      onSync(await api.removeConfigRemote())
      connectedWarnings = []
      await load()
    })
  }
</script>

<div class="remote">
  <h1>Remote</h1>
  <p class="muted small">
    Your guidance, projects and their memory live in a git repo on this machine. With a remote, every change syncs to it
    and your other machines pick it up. Templates and secrets never leave this machine.
  </p>

  {#if !loaded}
    <p class="muted">Loading…</p>
  {:else if remote}
    <section class="card">
      <dl>
        <dt>URL</dt>
        <dd><code>{remote}</code></dd>
        <dt>Visibility</dt>
        <dd>
          <span class:warn={visibility !== 'private'}>{visibility}</span>
          {#if !unconfirmed && sync?.visibility}<span class="muted">when it was set</span>{/if}
        </dd>
        <dt>Last sync</dt>
        <dd>{formatSyncTime(sync?.last_sync ?? null)}</dd>
      </dl>
      {#if unconfirmed}
        <p class="warn small">
          This remote wasn't connected through Tworky, so your config doesn't sync with it. Connect it to check who can
          see it and start syncing, or remove it.
        </p>
      {:else if visibility !== 'private'}
        <p class="warn small">
          {visibility === 'unknown'
            ? "Tworky couldn't tell who can see this repo. Make sure it's private."
            : `This repo is ${visibility}: more people than you can read your guidance, project config and memory.`}
        </p>
      {/if}
      <RemoteWarnings warnings={unconfirmed ? (adoption.check?.warnings ?? []) : connectedWarnings} />
      <div class="actions">
        <button class="danger" disabled={busy} onclick={remove}>Remove</button>
        {#if unconfirmed}
          <button class="primary" disabled={busy} onclick={adopt}>
            {adoption.check ? 'Connect it anyway' : 'Connect it'}
          </button>
        {/if}
      </div>
    </section>
  {:else}
    <section class="card">
      <h2>Create a private repo</h2>
      <p class="muted small">
        Creates a private <code>tworky-config</code> repo on GitHub (<code>tworky-config-dev</code> from a dev build) and
        syncs your config with it.
      </p>
      {#if owners === null}
        <p class="muted small">Loading your GitHub accounts…</p>
      {:else}
        <label class="row">
          <span>Owner</span>
          <select bind:value={owner} disabled={busy || !owners.length}>
            {#each owners as o, i (o)}<option value={o}>{o}{i === 0 ? ' (you)' : ''}</option>{/each}
          </select>
          <button class="primary" disabled={busy || !owner} onclick={create}>Create private repo</button>
        </label>
        {#if ownersError}<p class="warn small">{ownersError}</p>{/if}
        {#if ownerIsOrg}
          <p class="warn small">
            {owner} is an organisation: anyone who can push to its repo can change the hooks, services and guidance Tworky
            runs on this machine.
          </p>
        {/if}
      {/if}
    </section>

    <section class="card">
      <h2>Use an existing repo</h2>
      <p class="muted small">
        A config another machine already syncs, or an empty repo. When both have config, Tworky merges them, and any
        conflict goes to the Sync tab.
      </p>
      <label class="row">
        <input bind:value={typed} placeholder="owner/tworky-config or a git URL" disabled={busy} />
        <button class="primary" disabled={busy || !typed.trim()} onclick={useExisting}>
          {confirmation.check ? 'Use it anyway' : 'Use this repo'}
        </button>
      </label>
      <RemoteWarnings warnings={confirmation.check?.warnings ?? []} />
      {#if confirmation.check?.needs_confirmation}
        <p class="warn small">Read the warnings above. Use it anyway only if you're sure.</p>
      {/if}
    </section>
  {/if}

  {#if busy}<p class="muted small">Working…</p>{/if}
  {#if error}<pre class="error">{error}</pre>{/if}
</div>

<style>
  .remote {
    padding: 16px 24px 32px;
    max-width: 760px;
    display: flex;
    flex-direction: column;
    gap: 12px;
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
  .small {
    font-size: 12px;
    margin: 0;
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
  .row {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .row input {
    flex: 1;
    min-width: 0;
  }
  .actions {
    display: flex;
    justify-content: flex-end;
  }
  .warn {
    color: var(--warn);
  }
  .error {
    margin: 0;
    white-space: pre-wrap;
    color: var(--bad);
  }
</style>
