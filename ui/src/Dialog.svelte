<script lang="ts">
  import { onMount, type Snippet } from 'svelte'

  interface Props {
    title: string
    subtitle?: string
    busy?: boolean
    wide?: boolean
    onClose: () => void
    children: Snippet
    footer: Snippet
  }

  let { title, subtitle, busy = false, wide = false, onClose, children, footer }: Props = $props()

  let root = $state<HTMLDivElement>()

  onMount(() => root?.querySelector<HTMLInputElement>('input:not([type=radio]), select')?.focus())
</script>

<div
  class="backdrop"
  role="dialog"
  aria-modal="true"
  aria-label={title}
  tabindex="-1"
  bind:this={root}
  onkeydown={(e) => e.key === 'Escape' && !busy && onClose()}
  onclick={(e) => e.target === e.currentTarget && !busy && onClose()}
>
  <div class="panel" class:wide>
    <header>
      <h1>{title}</h1>
      {#if subtitle}<span class="muted">{subtitle}</span>{/if}
    </header>
    {@render children()}
    <footer>{@render footer()}</footer>
  </div>
</div>

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    z-index: 20;
    display: flex;
    align-items: flex-start;
    justify-content: center;
    padding-top: 8vh;
    background: color-mix(in srgb, #000 60%, transparent);
    outline: none;
  }
  .panel {
    width: min(640px, calc(100vw - 32px));
    max-height: 84vh;
    display: flex;
    flex-direction: column;
    gap: 10px;
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 10px;
    padding: 16px 18px;
    box-shadow: 0 10px 40px rgb(0 0 0 / 0.4);
  }
  .panel.wide {
    width: min(860px, calc(100vw - 32px));
  }
  header {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  h1 {
    font-size: 16px;
    margin: 0;
    font-weight: 600;
  }
  footer {
    display: flex;
    justify-content: flex-end;
    align-items: center;
    gap: 8px;
  }
  .panel :global(input:not([type='radio']):not([type='checkbox'])),
  .panel :global(select) {
    font: inherit;
    color: var(--text);
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 5px 8px;
  }
  .panel :global(.error) {
    margin: 0;
    color: var(--bad);
    white-space: pre-wrap;
    max-height: 120px;
    overflow: auto;
  }
  .panel :global(.grid) {
    flex: 1;
    min-height: 200px;
    overflow: auto;
    border: 1px solid var(--border);
    border-radius: 6px;
  }
  .panel :global(.grid table) {
    width: 100%;
    border-collapse: collapse;
    font-size: 12px;
  }
  .panel :global(.grid th) {
    position: sticky;
    top: 0;
    background: var(--panel-2);
    text-align: left;
    font-weight: 600;
    padding: 4px 8px;
  }
  .panel :global(.grid td) {
    padding: 4px 8px;
    border-top: 1px solid var(--border);
    white-space: nowrap;
  }
  .panel :global(.grid td.title) {
    white-space: normal;
  }
  .panel :global(.grid tr.row) {
    cursor: pointer;
  }
  .panel :global(.grid tr.row:hover) {
    background: var(--panel-2);
  }
  .panel :global(.grid tr.picked) {
    background: color-mix(in srgb, var(--accent) 18%, transparent);
  }
  .panel :global(.grid tr.listed) {
    opacity: 0.5;
    cursor: default;
  }
  .panel :global(.grid .note) {
    padding: 12px 8px;
  }
</style>
