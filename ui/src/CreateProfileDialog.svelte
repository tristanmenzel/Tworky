<script lang="ts">
  import { untrack } from 'svelte'
  import { api } from './api'
  import Dialog from './Dialog.svelte'
  import ServiceList, { type ServiceLink } from './ServiceList.svelte'
  import { overlay } from './overlay.svelte'

  interface Props {
    project: string
    services: ServiceLink[]
    initialName?: string
    initialEmail?: string
    initialService?: string
    onCreated: (name: string) => void
    onClose: () => void
  }

  let { project, services, initialName, initialEmail, initialService, onCreated, onClose }: Props = $props()

  let name = $state(untrack(() => initialName ?? ''))
  let email = $state(untrack(() => initialEmail ?? ''))
  const requestedMissing = untrack(
    () => initialService !== undefined && !services.some((s) => s.name === initialService),
  )
  let picked = $state<string | null>(
    untrack(() =>
      initialService !== undefined
        ? requestedMissing
          ? null
          : initialService
        : services.length === 1
          ? services[0].name
          : null,
    ),
  )
  let saving = $state(false)
  let error = $state<string | null>(null)

  const ready = $derived(name.trim() !== '' && email.trim() !== '' && picked !== null && !saving)

  $effect(() => {
    overlay.open++
    return () => overlay.open--
  })

  async function create() {
    if (!ready) return
    saving = true
    error = null
    try {
      await api.saveAccount(project, null, {
        name: name.trim(),
        username: email.trim(),
        start_url: '',
        start_service: picked,
        mobile: false,
        login_origins: [],
        color: null,
      })
      onCreated(name.trim())
    } catch (e) {
      error = String(e)
    } finally {
      saving = false
    }
  }
</script>

<Dialog
  title="Create a profile"
  subtitle="A test account for {project}. Its tab offers to fill the email on login pages and remembers where it signs in."
  busy={saving}
  {onClose}
>
  <label class="field">
    <span>Name</span>
    <input bind:value={name} placeholder="Tester 2" disabled={saving} />
  </label>
  <label class="field">
    <span>Email</span>
    <input
      bind:value={email}
      type="email"
      placeholder="tester2@example.com"
      disabled={saving}
      onkeydown={(e) => e.key === 'Enter' && create()}
    />
  </label>
  <span class="muted">Start on</span>
  {#if requestedMissing}
    <span class="muted">{initialService}, the service Claude asked for, isn't available on this worker.</span>
  {/if}
  <ServiceList {services} bind:picked disabled={saving} />
  {#if error}<pre class="error">{error}</pre>{/if}
  {#snippet footer()}
    <button onclick={onClose} disabled={saving}>Cancel</button>
    <button class="primary" disabled={!ready} onclick={create}>{saving ? 'Creating…' : 'OK'}</button>
  {/snippet}
</Dialog>

<style>
  .field {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .field span {
    width: 48px;
  }
  .field input {
    flex: 1;
  }
</style>
