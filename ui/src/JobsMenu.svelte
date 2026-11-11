<script lang="ts">
  import { formatBytes, jobLabel } from './issue'
  import { overlay } from './overlay.svelte'

  export interface JobEntry {
    tab: string
    command: string
    memory_bytes: number
  }

  let { jobs, current, onSelect }: { jobs: JobEntry[]; current: string; onSelect: (tab: string) => void } = $props()

  let open = $state(false)
  let root: HTMLDivElement
  let trigger: HTMLButtonElement
  let pos = $state({ left: 0, top: 0 })

  const active = $derived(jobs.find((j) => j.tab === current))
  const total = $derived(jobs.reduce((sum, j) => sum + j.memory_bytes, 0))

  $effect(() => {
    if (!open) return
    overlay.open++
    return () => overlay.open--
  })

  function toggle() {
    if (!open) {
      const r = trigger.getBoundingClientRect()
      pos = { left: r.left, top: r.bottom + 4 }
    }
    open = !open
  }

  function choose(tab: string) {
    open = false
    onSelect(tab)
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

<div class="tab" class:active={!!active} bind:this={root}>
  <button bind:this={trigger} aria-haspopup="menu" aria-expanded={open} onclick={toggle}>
    <span class="run">▶</span>
    dev ({jobs.length}){#if active}
      · {jobLabel(active.command)}{/if}
    <span class="muted mem">{formatBytes(total)}</span>
  </button>
  {#if open}
    <div class="popup" role="menu" style:left="{pos.left}px" style:top="{pos.top}px">
      {#each jobs as j (j.tab)}
        <button role="menuitemradio" aria-checked={j.tab === current} class:current={j.tab === current} title={j.command} onclick={() => choose(j.tab)}>
          <span class="check">{j.tab === current ? '✓' : ''}</span>
          <span class="name">{jobLabel(j.command)}</span>
          <span class="muted mem">{formatBytes(j.memory_bytes)}</span>
        </button>
      {/each}
    </div>
  {/if}
</div>

<style>
  .tab {
    display: flex;
    align-items: center;
    border-radius: 6px 6px 0 0;
    border: 1px solid transparent;
    border-bottom: 0;
  }
  .tab.active {
    background: var(--bg);
    border-color: var(--border);
  }
  .tab > button {
    background: none;
    border: 0;
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .run {
    color: var(--muted);
    font-size: 10px;
  }
  .popup {
    position: fixed;
    z-index: 20;
    min-width: 200px;
    max-width: 320px;
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 4px;
    box-shadow: 0 6px 20px rgb(0 0 0 / 0.18);
    display: flex;
    flex-direction: column;
  }
  .popup button {
    background: none;
    border: 0;
    text-align: left;
    padding: 6px 8px;
    border-radius: 5px;
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .popup button:hover {
    background: var(--panel-2);
  }
  .check {
    width: 12px;
    flex: none;
  }
  .name {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .mem {
    font-size: 11px;
  }
</style>
