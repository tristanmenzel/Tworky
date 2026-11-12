<script lang="ts">
  import { onMount, untrack } from 'svelte'
  import { listen } from '@tauri-apps/api/event'
  import { api, type KeepAwakeMode, type KeepAwakeStatus } from './api'
  import { overlay } from './overlay.svelte'

  const LOCKS: Record<KeepAwakeStatus['lock'], string> = {
    windows: 'Windows power request',
    macos: 'macOS assertion',
    portal: 'desktop portal',
    logind: 'logind',
    none: '',
  }

  let status = $state<KeepAwakeStatus>()
  let open = $state(false)
  let error = $state('')
  let root = $state<HTMLSpanElement>()
  let trigger = $state<HTMLButtonElement>()
  let pos = $state({ right: 0, top: 0 })

  const choices = $derived<{ label: string; mode: KeepAwakeMode; checked: boolean }[]>(
    status?.has_battery
      ? [
          { label: 'Off', mode: 'off', checked: status.mode === 'off' },
          { label: 'Plugged in', mode: 'plugged_in', checked: status.mode === 'plugged_in' },
          { label: 'Always', mode: 'always', checked: status.mode === 'always' },
        ]
      : [
          { label: 'Off', mode: 'off', checked: status?.mode === 'off' },
          { label: 'On', mode: 'plugged_in', checked: status?.mode !== 'off' },
        ],
  )

  const working = $derived.by(() => {
    if (!status) return ''
    const n = status.busy_sessions
    const sessions = n === 0 ? 'No sessions working' : n === 1 ? '1 session working' : `${n} sessions working`
    const power = status.on_battery === null ? '' : status.on_battery ? 'on battery' : 'on AC'
    return [sessions, power].filter(Boolean).join(' · ')
  })

  const statusLine = $derived(status ? [working, LOCKS[status.lock]].filter(Boolean).join(' · ') : '')

  onMount(() => {
    let heard = false
    const unlisten = listen<KeepAwakeStatus>('keep-awake-changed', (e) => {
      heard = true
      status = e.payload
      error = ''
    })
    unlisten
      .then(() => api.keepAwakeStatus())
      .then((s) => {
        if (!heard) status = s
      })
      .catch(() => {})
    return () => void unlisten.then((f) => f())
  })

  $effect(() => {
    if (!open) return
    untrack(() => overlay.open++)
    return () => overlay.open--
  })

  function toggle() {
    if (!open) {
      const r = trigger!.getBoundingClientRect()
      pos = { right: window.innerWidth - r.right, top: r.bottom + 4 }
    }
    open = !open
  }

  async function choose(mode: KeepAwakeMode) {
    open = false
    error = ''
    try {
      await api.setKeepAwake(mode)
    } catch (e) {
      error = String(e)
    }
  }
</script>

<svelte:window
  onclick={(e) => {
    if (open && !root?.contains(e.target as Node)) open = false
  }}
  onkeydown={(e) => {
    if (open && e.key === 'Escape') open = false
  }}
  onresize={() => (open = false)}
/>

{#if status}
  <span class="keep-awake" bind:this={root}>
    <button
      bind:this={trigger}
      class="pill"
      class:ok={status.holding}
      class:muted={!status.holding}
      class:warn={!!error}
      aria-haspopup="menu"
      aria-expanded={open}
      title={error ? `Couldn't change keep awake: ${error}` : working}
      onclick={toggle}
    >
      {status.holding ? `☕ Awake · ${status.busy_sessions}` : '☾ Sleep ok'}
    </button>
    {#if open}
      <div class="popup" role="menu" style:right="{pos.right}px" style:top="{pos.top}px">
        <div class="heading muted">Keep computer awake</div>
        {#each choices as c (c.label)}
          <button role="menuitemradio" aria-checked={c.checked} onclick={() => choose(c.mode)}>
            <span class="check">{c.checked ? '✓' : ''}</span>
            {c.label}
          </button>
        {/each}
        <div class="status muted">{statusLine}</div>
      </div>
    {/if}
  </span>
{/if}

<style>
  .keep-awake {
    display: inline-flex;
    align-items: center;
  }
  .pill {
    font-size: 12px;
    font-weight: 600;
    padding: 1px 8px;
    border-radius: 10px;
    white-space: nowrap;
    border: 1px solid transparent;
    background: none;
    font-family: inherit;
    cursor: pointer;
  }
  .pill.ok {
    color: var(--ok);
    border-color: color-mix(in srgb, var(--ok) 55%, transparent);
  }
  .pill.muted {
    color: var(--muted);
    border-color: var(--border);
    font-weight: 400;
  }
  .pill.warn {
    color: var(--warn);
    border-color: color-mix(in srgb, var(--warn) 55%, transparent);
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
  .heading,
  .status {
    font-size: 11px;
    padding: 4px 8px;
  }
  .status {
    border-top: 1px solid var(--border);
    margin-top: 4px;
    padding-top: 6px;
  }
</style>
