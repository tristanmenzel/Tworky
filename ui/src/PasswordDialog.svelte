<script lang="ts">
  import Dialog from './Dialog.svelte'
  import { overlay } from './overlay.svelte'

  interface Props {
    account: string
    onSave: (password: string) => Promise<void>
    onClose: () => void
  }

  let { account, onSave, onClose }: Props = $props()

  let password = $state('')
  let saving = $state(false)
  let error = $state<string | null>(null)

  $effect(() => {
    overlay.open++
    return () => overlay.open--
  })

  async function save() {
    if (!password) return
    saving = true
    error = null
    try {
      await onSave(password)
    } catch (e) {
      error = String(e)
    } finally {
      saving = false
    }
  }
</script>

<Dialog
  title="Set {account}'s password"
  subtitle="Kept in the OS credential store, then filled into this form. Claude never sees it."
  busy={saving}
  {onClose}
>
  <input
    type="password"
    bind:value={password}
    placeholder="Password"
    aria-label="Password"
    autocomplete="off"
    disabled={saving}
    onkeydown={(e) => e.key === 'Enter' && save()}
  />
  {#if error}<pre class="error">{error}</pre>{/if}
  {#snippet footer()}
    <button onclick={onClose} disabled={saving}>Cancel</button>
    <button class="primary" disabled={!password || saving} onclick={save}>{saving ? 'Saving…' : 'OK'}</button>
  {/snippet}
</Dialog>
