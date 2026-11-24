<script lang="ts">
  import { untrack } from 'svelte'
  import type { Viewport } from './api'
  import { overlay } from './overlay.svelte'

  const MIN_SIZE = 200
  const MAX_SIZE = 3840

  const PRESETS: { label: string; viewport: Viewport }[] = [
    { label: 'Fit', viewport: { kind: 'fit' } },
    { label: 'Phone', viewport: { kind: 'fixed', width: 393, height: 852, mobile: true } },
    { label: 'Tablet', viewport: { kind: 'fixed', width: 820, height: 1180, mobile: true } },
    { label: 'Laptop', viewport: { kind: 'fixed', width: 1280, height: 800, mobile: false } },
    { label: 'Desktop', viewport: { kind: 'fixed', width: 1440, height: 900, mobile: false } },
  ]

  let { viewport, scale, onSelect }: { viewport: Viewport; scale: number; onSelect: (viewport: Viewport) => void } =
    $props()

  let open = $state(false)
  let custom = $state(false)
  let width = $state(1280)
  let height = $state(800)
  let mobile = $state(false)
  let root: HTMLSpanElement
  let trigger: HTMLButtonElement
  let pos = $state({ right: 0, top: 0 })

  const preset = $derived(PRESETS.find((p) => same(p.viewport, viewport)))
  const name = $derived(preset?.label ?? (viewport.kind === 'fixed' ? `${viewport.width}×${viewport.height}` : 'Fit'))
  const percent = $derived(viewport.kind === 'fixed' && scale > 0 && scale < 1 ? `${Math.round(scale * 100)}%` : '')
  const title = $derived(
    viewport.kind === 'fit'
      ? 'Viewport: fits the pane'
      : `Viewport: ${viewport.width}×${viewport.height}${viewport.mobile ? ', mobile' : ''}${percent ? `, shown at ${percent}` : ''}`,
  )
  const valid = $derived(inRange(width) && inRange(height))

  $effect(() => {
    if (!open) return
    untrack(() => overlay.open++)
    return () => overlay.open--
  })

  function same(a: Viewport, b: Viewport) {
    if (a.kind === 'fit' || b.kind === 'fit') return a.kind === b.kind
    return a.width === b.width && a.height === b.height && a.mobile === b.mobile
  }

  function inRange(n: number) {
    return Number.isInteger(n) && n >= MIN_SIZE && n <= MAX_SIZE
  }

  function toggle() {
    if (!open) {
      const r = trigger.getBoundingClientRect()
      pos = { right: window.innerWidth - r.right, top: r.bottom + 4 }
      custom = false
      if (viewport.kind === 'fixed') ({ width, height, mobile } = viewport)
    }
    open = !open
  }

  function choose(next: Viewport) {
    open = false
    if (!same(next, viewport)) onSelect(next)
  }

  function apply() {
    if (valid) choose({ kind: 'fixed', width, height, mobile })
  }
</script>

<svelte:window
  onclick={(e) => {
    if (open && !root.contains(e.target as Node)) open = false
  }}
  onkeydown={(e) => {
    if (open && e.key === 'Escape') open = false
  }}
  onresize={() => (open = false)}
/>

<span class="picker" bind:this={root}>
  <button bind:this={trigger} class="trigger" aria-haspopup="menu" aria-expanded={open} {title} onclick={toggle}>
    {name}{#if percent}<span class="muted"> · {percent}</span>{/if}
  </button>
  {#if open}
    <div class="popup" role="menu" style:right="{pos.right}px" style:top="{pos.top}px">
      <div class="heading muted">Viewport</div>
      {#each PRESETS as p (p.label)}
        <button role="menuitemradio" aria-checked={p === preset} onclick={() => choose(p.viewport)}>
          <span class="check">{p === preset ? '✓' : ''}</span>
          <span class="name">{p.label}</span>
          {#if p.viewport.kind === 'fixed'}
            <span class="muted size">{p.viewport.width}×{p.viewport.height}{p.viewport.mobile ? ' mobile' : ''}</span>
          {/if}
        </button>
      {/each}
      <button role="menuitemradio" aria-checked={!preset} onclick={() => (custom = !custom)}>
        <span class="check">{preset ? '' : '✓'}</span>
        <span class="name">Custom…</span>
      </button>
      {#if custom}
        <form
          class="custom"
          onsubmit={(e) => {
            e.preventDefault()
            apply()
          }}
        >
          <div class="size-fields">
            <input type="number" aria-label="Width" min={MIN_SIZE} max={MAX_SIZE} step="1" bind:value={width} />
            <span class="muted">×</span>
            <input type="number" aria-label="Height" min={MIN_SIZE} max={MAX_SIZE} step="1" bind:value={height} />
          </div>
          <label><input type="checkbox" bind:checked={mobile} /> Mobile</label>
          {#if !valid}<div class="muted hint">Width and height from {MIN_SIZE} to {MAX_SIZE}</div>{/if}
          <button type="submit" class="primary" disabled={!valid}>Apply</button>
        </form>
      {/if}
    </div>
  {/if}
</span>

<style>
  .picker {
    display: inline-flex;
    flex: none;
  }
  .trigger {
    border-radius: 16px;
    font-size: 12px;
    padding: 4px 10px;
    white-space: nowrap;
  }
  .popup {
    position: fixed;
    z-index: 20;
    min-width: 220px;
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 4px;
    box-shadow: 0 6px 20px rgb(0 0 0 / 0.18);
    display: flex;
    flex-direction: column;
  }
  .popup > button {
    background: none;
    border: 0;
    text-align: left;
    padding: 6px 8px;
    border-radius: 5px;
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .popup > button:hover {
    background: var(--panel-2);
  }
  .check {
    width: 12px;
    flex: none;
  }
  .name {
    flex: 1;
  }
  .size,
  .heading,
  .hint {
    font-size: 11px;
  }
  .heading {
    padding: 4px 8px;
  }
  .custom {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 6px 8px 8px 26px;
  }
  .size-fields {
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .size-fields input {
    width: 72px;
    font: inherit;
    color: var(--text);
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 3px 6px;
  }
  .custom label {
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .custom button {
    align-self: flex-start;
  }
</style>
