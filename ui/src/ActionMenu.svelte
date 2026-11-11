<script lang="ts">
  export type MenuItem =
    | {
        label: string
        danger?: boolean
        /** A reason the action isn't available; shown as the item's tooltip. */
        disabled?: string | false
        onSelect: () => void
        separator?: false
      }
    | { separator: true }

  let {
    items,
    label = 'Actions',
    trigger = '⋯',
  }: { items: MenuItem[]; label?: string; trigger?: string } = $props()

  let open = $state(false)
  let root: HTMLDivElement

  function choose(item: { onSelect: () => void }) {
    open = false
    item.onSelect()
  }
</script>

<svelte:window
  onclick={(e) => {
    if (open && !root.contains(e.target as Node)) open = false
  }}
  onkeydown={(e) => {
    if (open && e.key === 'Escape') open = false
  }}
/>

<div class="menu" bind:this={root}>
  <button class="trigger" class:icon={trigger === '⋯'} aria-label={label} aria-haspopup="menu" aria-expanded={open} onclick={() => (open = !open)}
    >{trigger}</button
  >
  {#if open}
    <div class="popup" role="menu">
      {#each items as item, i (i)}
        {#if item.separator}
          <hr />
        {:else}
          <button
            role="menuitem"
            class:danger={item.danger}
            disabled={!!item.disabled}
            title={item.disabled || ''}
            onclick={() => choose(item)}>{item.label}</button
          >
          {#if item.disabled}<div class="reason muted">{item.disabled}</div>{/if}
        {/if}
      {/each}
    </div>
  {/if}
</div>

<style>
  .menu {
    position: relative;
    display: inline-block;
  }
  .trigger.icon {
    padding: 0 8px;
    line-height: 20px;
    font-size: 14px;
  }
  .popup {
    position: absolute;
    right: 0;
    top: calc(100% + 4px);
    z-index: 5;
    min-width: 180px;
    max-width: 280px;
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
  }
  .popup button:hover:not(:disabled) {
    background: var(--panel-2);
  }
  .popup button.danger:not(:disabled) {
    color: var(--bad);
  }
  hr {
    width: 100%;
    margin: 4px 0;
    border: 0;
    border-top: 1px solid var(--border);
  }
  .reason {
    font-size: 11px;
    padding: 0 8px 6px;
    white-space: normal;
  }
</style>
