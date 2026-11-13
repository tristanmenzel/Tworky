<script lang="ts">
  import { answer, confirmation } from './confirm.svelte'

  let cancelEl = $state<HTMLButtonElement>()

  const request = $derived(confirmation.request)
  const warningLines = $derived(
    request?.warning?.message === undefined ? [] : [request.warning.message].flat(),
  )

  $effect(() => {
    if (request) cancelEl?.focus()
  })

  function onKeydown(e: KeyboardEvent) {
    if (!request || e.key !== 'Escape') return
    e.preventDefault()
    e.stopPropagation()
    answer(false)
  }
</script>

<svelte:window onkeydown={onKeydown} />

{#if request}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <div
    class="backdrop"
    role="dialog"
    aria-modal="true"
    aria-label={request.title}
    tabindex="-1"
    onclick={(e) => e.target === e.currentTarget && answer(false)}
  >
    <div class="panel" class:warn={request.warning}>
      <h1>{request.title}</h1>
      {#if request.message}<p>{request.message}</p>{/if}
      {#if request.warning}
        <div class="warning">
          <strong>{request.warning.title}</strong>
          {#each warningLines as line, i (i)}<p>{line}</p>{/each}
        </div>
      {/if}
      <footer>
        <button bind:this={cancelEl} onclick={() => answer(false)}>Cancel</button>
        <button class={request.danger ? 'bad' : 'primary'} onclick={() => answer(true)}>{request.confirmLabel}</button>
      </footer>
    </div>
  </div>
{/if}

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    z-index: 30;
    display: flex;
    align-items: flex-start;
    justify-content: center;
    padding-top: 8vh;
    background: color-mix(in srgb, #000 60%, transparent);
    outline: none;
  }
  .panel {
    width: min(480px, calc(100vw - 32px));
    display: flex;
    flex-direction: column;
    gap: 10px;
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 10px;
    padding: 16px 18px;
    box-shadow: 0 10px 40px rgb(0 0 0 / 0.4);
  }
  .panel.warn {
    border-top: 3px solid var(--warn);
  }
  h1 {
    font-size: 16px;
    margin: 0;
    font-weight: 600;
  }
  p {
    margin: 0;
    white-space: pre-wrap;
  }
  .warning {
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 8px 10px;
    border: 1px solid color-mix(in srgb, var(--warn) 45%, transparent);
    border-radius: 6px;
    background: color-mix(in srgb, var(--warn) 12%, transparent);
  }
  .warning strong {
    color: var(--warn);
  }
  footer {
    display: flex;
    justify-content: flex-end;
    align-items: center;
    gap: 8px;
  }
  button.bad {
    background: var(--bad);
    border-color: var(--bad);
    color: #fff;
  }
  button.bad:hover:not(:disabled) {
    border-color: var(--bad);
  }
</style>
