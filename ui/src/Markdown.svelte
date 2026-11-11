<script lang="ts" module>
  import DOMPurify from 'dompurify'
  import { convertFileSrc } from '@tauri-apps/api/core'

  const GITHUB_ASSET = /^https:\/\/([a-z0-9-]+\.)*(github\.com|githubusercontent\.com)\//i

  DOMPurify.addHook('afterSanitizeAttributes', (node) => {
    const src = node.tagName === 'IMG' ? node.getAttribute('src') : null
    if (!src) return
    node.setAttribute('data-original', src)
    if (GITHUB_ASSET.test(src)) node.setAttribute('src', convertFileSrc(src, 'ghimg'))
  })
</script>

<script lang="ts">
  import { marked } from 'marked'
  import { openUrl } from '@tauri-apps/plugin-opener'
  import ImageViewer from './ImageViewer.svelte'

  let { source, empty = '_No description._' }: { source: string; empty?: string } = $props()

  const html = $derived(DOMPurify.sanitize(marked.parse(source || empty, { async: false }) as string))

  let viewing = $state<{ src: string; alt: string; original: string } | null>(null)

  function onClick(e: MouseEvent) {
    const target = e.target as HTMLElement
    if (target instanceof HTMLImageElement) {
      e.preventDefault()
      viewing = { src: target.src, alt: target.alt, original: target.dataset.original ?? target.src }
      return
    }
    const a = target.closest('a')
    if (a?.href) {
      e.preventDefault()
      openUrl(a.href)
    }
  }
</script>

<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
<div class="md" onclick={onClick}>{@html html}</div>

{#if viewing}
  <ImageViewer {...viewing} onClose={() => (viewing = null)} />
{/if}

<style>
  .md {
    overflow-wrap: anywhere;
  }
  .md :global(:first-child) {
    margin-top: 0;
  }
  /* GitHub's <img> tags carry the original width and height attributes; `height: auto` keeps the
     aspect ratio once the width is capped. */
  .md :global(img) {
    max-width: min(100%, 640px);
    max-height: 360px;
    width: auto;
    height: auto;
    object-fit: contain;
    cursor: zoom-in;
    border: 1px solid var(--border);
    border-radius: 4px;
    vertical-align: top;
  }
  .md :global(pre) {
    background: var(--panel-2);
    padding: 8px;
    border-radius: 6px;
    overflow: auto;
  }
  .md :global(table) {
    border-collapse: collapse;
  }
  .md :global(td),
  .md :global(th) {
    border: 1px solid var(--border);
    padding: 4px 8px;
  }
</style>
