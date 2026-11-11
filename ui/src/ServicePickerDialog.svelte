<script lang="ts">
  import Dialog from './Dialog.svelte'
  import ServiceList, { type ServiceLink } from './ServiceList.svelte'
  import { overlay } from './overlay.svelte'

  interface Props {
    services: ServiceLink[]
    onPick: (service: ServiceLink) => void
    onClose: () => void
  }

  let { services, onPick, onClose }: Props = $props()

  let picked = $state<string | null>(null)

  const chosen = $derived(services.find((s) => s.name === picked))

  $effect(() => {
    overlay.open++
    return () => overlay.open--
  })
</script>

<Dialog
  title="Open an anonymous tab"
  subtitle="Signed in as nobody, on a fresh profile discarded when the tab closes."
  {onClose}
>
  <ServiceList {services} bind:picked />
  {#snippet footer()}
    <button onclick={onClose}>Cancel</button>
    <button class="primary" disabled={!chosen} onclick={() => chosen && onPick(chosen)}>OK</button>
  {/snippet}
</Dialog>
