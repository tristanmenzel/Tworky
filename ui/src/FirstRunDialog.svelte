<script lang="ts">
  import { api, type RemoteCheck, type SharedConfig } from './api'
  import Dialog from './Dialog.svelte'
  import { overlay } from './overlay.svelte'

  interface Props {
    onDone: () => void
  }

  let { onDone }: Props = $props()

  let step = $state<'choose' | 'shared'>('choose')
  let found = $state<SharedConfig[] | null>(null)
  let finding = $state(false)
  let picked = $state<string | null>(null)
  let typed = $state('')
  let check = $state<RemoteCheck | null>(null)
  let busy = $state(false)
  let error = $state<string | null>(null)

  const remote = $derived(typed.trim() || picked || '')

  $effect(() => {
    overlay.open++
    return () => overlay.open--
  })

  $effect(() => {
    void remote
    check = null
  })

  async function startFresh() {
    busy = true
    error = null
    try {
      await api.startFreshConfig()
      onDone()
    } catch (e) {
      error = String(e)
    } finally {
      busy = false
    }
  }

  async function showShared() {
    step = 'shared'
    error = null
    if (found) return
    finding = true
    try {
      found = await api.findSharedConfigs()
      if (found.length === 1) picked = found[0].url
    } catch (e) {
      found = []
      error = `Couldn't look for your shared config on GitHub: ${e}`
    } finally {
      finding = false
    }
  }

  async function useShared() {
    if (!remote) return
    busy = true
    error = null
    try {
      if (!check) {
        const checked = await api.checkConfigRemote(remote)
        if (checked.warnings.length) {
          check = checked
          return
        }
      }
      await api.useSharedConfig(remote, check?.needs_confirmation ?? false)
      onDone()
    } catch (e) {
      error = String(e)
    } finally {
      busy = false
    }
  }
</script>

<Dialog
  title="Set up Tworky's config"
  subtitle="Your guidance, projects and their memory live in a git repo that can sync with a private GitHub repo."
  {busy}
  onClose={() => {}}
>
  {#if step === 'choose'}
    <div class="options">
      <button class="option" disabled={busy} onclick={startFresh}>
        <strong>Start fresh</strong>
        <span class="muted">An empty config. You can sync it with a GitHub repo later.</span>
      </button>
      <button class="option" disabled={busy} onclick={showShared}>
        <strong>Use my shared config</strong>
        <span class="muted">Clone the config another machine already syncs with GitHub.</span>
      </button>
    </div>
  {:else}
    {#if finding}
      <p class="muted">Looking for your config repos on GitHub…</p>
    {:else if found?.length}
      {#each found as config (config.url)}
        <label class="choice">
          <input type="radio" name="shared" value={config.url} bind:group={picked} disabled={busy} />
          <strong>{config.owner}</strong>
          <span class="muted">{config.url}</span>
          {#if config.visibility !== 'private'}<span class="tag">{config.visibility}</span>{/if}
        </label>
      {/each}
    {:else if found}
      <p class="muted">No config repo found for your account or your organisations.</p>
    {/if}
    <label class="url">
      <span>Or a URL or owner/name</span>
      <input bind:value={typed} placeholder="https://github.com/me/tworky-config" disabled={busy} />
    </label>
    {#if check}
      <ul class="warnings">
        {#each check.warnings as warning (warning)}<li>{warning}</li>{/each}
      </ul>
    {/if}
  {/if}

  {#if error}<pre class="error">{error}</pre>{/if}

  {#snippet footer()}
    {#if step === 'shared'}
      <button disabled={busy} onclick={() => (step = 'choose')}>Back</button>
      <button class="primary" disabled={busy || !remote} onclick={useShared}>
        {busy ? 'Setting up…' : check ? 'Use it anyway' : 'Use this config'}
      </button>
    {/if}
  {/snippet}
</Dialog>

<style>
  .options {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .option {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 2px;
    padding: 10px 12px;
    text-align: left;
  }
  .choice {
    display: flex;
    align-items: baseline;
    gap: 8px;
  }
  .url {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .warnings {
    margin: 0;
    padding-left: 18px;
    color: var(--warn);
  }
</style>
