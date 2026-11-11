<script lang="ts">
  interface Props {
    value: string | null
    options: { value: string; label: string }[]
    /** Shown, unselectable, when `value` is null or not among `options`. */
    placeholder: string
    label: string
    tone: string
    onChange: (value: string) => Promise<void>
  }

  let { value, options, placeholder, label, tone, onChange }: Props = $props()

  let selected = $state('')
  let saving = $state(false)
  let saved = $state(false)
  let savedTimer: ReturnType<typeof setTimeout> | undefined

  const known = $derived(options.some((o) => o.value === value))

  $effect(() => {
    selected = known ? value! : ''
  })

  async function change() {
    if (!selected) return
    saving = true
    saved = false
    clearTimeout(savedTimer)
    try {
      await onChange(selected)
      saved = true
      savedTimer = setTimeout(() => (saved = false), 1500)
    } catch {
      selected = known ? value! : ''
    } finally {
      saving = false
    }
  }
</script>

<span class="wrap">
  <select
    class={tone}
    bind:value={selected}
    disabled={saving}
    onchange={change}
    onclick={(e) => e.stopPropagation()}
    aria-label={label}
    title={label}
  >
    {#if !known}<option value="" disabled>{placeholder}</option>{/if}
    {#each options as o (o.value)}
      <option value={o.value}>{o.label}</option>
    {/each}
  </select>
  <span class="note muted" aria-live="polite">{saving ? 'Saving…' : saved ? 'Saved' : ''}</span>
</span>

<style>
  .wrap {
    display: inline-flex;
    align-items: center;
    gap: 4px;
  }
  select {
    font: inherit;
    font-size: 12px;
    color: var(--text);
    background: var(--panel-2);
    border: 1px solid var(--border);
    border-radius: 10px;
    padding: 1px 6px;
    cursor: pointer;
  }
  select:hover:not(:disabled) {
    border-color: var(--accent);
  }
  select.todo {
    color: var(--muted);
  }
  select.doing {
    color: var(--warn);
  }
  select.done {
    color: var(--ok);
  }
  select option {
    color: var(--text);
  }
  .note {
    font-size: 11px;
    min-width: 3.5em;
  }
</style>
