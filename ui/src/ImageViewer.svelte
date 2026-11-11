<script lang="ts">
  import { openUrl } from '@tauri-apps/plugin-opener'

  interface Props {
    src: string
    alt: string
    /** The image's own URL, when `src` is the authenticated `ghimg` proxy. */
    original: string
    onClose: () => void
  }

  let { src, alt, original, onClose }: Props = $props()

  let actual = $state(false)
  let dialog = $state<HTMLDivElement>()

  $effect(() => {
    dialog?.focus()
  })

  function key(e: KeyboardEvent) {
    if (e.key === 'Escape') onClose()
  }
</script>

<div
  class="backdrop"
  role="dialog"
  aria-modal="true"
  aria-label={alt || 'Image'}
  tabindex="-1"
  bind:this={dialog}
  onkeydown={key}
  onclick={(e) => e.target === e.currentTarget && onClose()}
>
  <div class="bar">
    <span class="muted">{alt && alt !== 'Image' ? alt : ''}</span>
    <button onclick={() => (actual = !actual)}>{actual ? 'Fit to window' : 'Actual size'}</button>
    <button onclick={() => openUrl(original)}>Open in browser</button>
    <button aria-label="Close" onclick={onClose}>✕</button>
  </div>
  <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
  <div class="stage" class:actual onclick={(e) => e.target === e.currentTarget && onClose()}>
    <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
    <img {src} {alt} onclick={() => (actual = !actual)} />
  </div>
</div>

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    z-index: 20;
    display: flex;
    flex-direction: column;
    background: color-mix(in srgb, #000 82%, transparent);
    outline: none;
  }
  .bar {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 8px 12px;
  }
  .bar .muted {
    margin-right: auto;
    color: #c9cfd6;
  }
  .stage {
    flex: 1;
    min-height: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 0 16px 16px;
  }
  .stage img {
    max-width: 100%;
    max-height: 100%;
    object-fit: contain;
    cursor: zoom-in;
    background: #fff;
    box-shadow: 0 6px 30px rgb(0 0 0 / 0.5);
  }
  .stage.actual {
    display: block;
    overflow: auto;
  }
  .stage.actual img {
    max-width: none;
    max-height: none;
    cursor: zoom-out;
  }
</style>
