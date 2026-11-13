<script lang="ts">
  import { api, type BrowserTabView } from './api'
  import { confirmOverBrowser } from './overlay.svelte'
  import PasswordDialog from './PasswordDialog.svelte'

  interface Props {
    tab: BrowserTabView
    visible: boolean
    onError: (message: string) => void
  }

  let { tab, visible, onError }: Props = $props()

  let host: HTMLDivElement
  let address: HTMLInputElement
  let url = $state('')
  let busy = $state(false)
  let settingPassword = $state(false)

  const needsPassword = $derived(tab.can_sign_in && tab.login_fields.includes('password') && !tab.has_password)

  $effect(() => {
    if (document.activeElement !== address) url = tab.url
  })

  async function call(work: () => Promise<unknown>) {
    try {
      await work()
    } catch (e) {
      onError(String(e))
    }
  }

  function syncBounds() {
    if (!visible || host.clientWidth === 0) return
    const r = host.getBoundingClientRect()
    void call(() => api.browserBounds(tab.id, { x: r.left, y: r.top, width: r.width, height: r.height }))
  }

  $effect(() => {
    if (visible) requestAnimationFrame(syncBounds)
  })

  $effect(() => {
    const observer = new ResizeObserver(syncBounds)
    observer.observe(host)
    window.addEventListener('resize', syncBounds)
    return () => {
      observer.disconnect()
      window.removeEventListener('resize', syncBounds)
    }
  })

  async function signIn() {
    busy = true
    await call(() => api.browserSignIn(tab.id))
    busy = false
  }

  async function fillEmail() {
    busy = true
    await call(async () => {
      const page = new URL(tab.url)
      if (['localhost', '127.0.0.1', '[::1]'].includes(page.hostname)) {
        await api.browserFillEmail(tab.id, true, page.origin)
        return
      }
      const origin = await api.browserFillEmail(tab.id, false)
      const allowed = await confirmOverBrowser({
        title: `Remember ${origin} as a login page for ${tab.account}?`,
        message: 'Its password can then be filled there, by you and by Claude.',
        confirmLabel: 'Remember',
      })
      if (allowed) await api.browserFillEmail(tab.id, true, origin)
    })
    busy = false
  }

  async function savePassword(password: string) {
    await api.setAccountPassword(tab.project, tab.account!, password)
    settingPassword = false
    await signIn()
  }

  async function startService() {
    busy = true
    await call(() => api.browserStartService(tab.id))
    busy = false
  }

  async function retry() {
    busy = true
    await call(() => api.browserRetry(tab.id))
    busy = false
  }

  async function endSession() {
    const ok = await confirmOverBrowser({
      title: `Sign ${tab.account} out of every site?`,
      message: 'Ends every session this browser holds for the account.',
      confirmLabel: 'End session',
      danger: true,
    })
    if (!ok) return
    busy = true
    await call(() => api.browserEndSession(tab.id))
    busy = false
  }
</script>

<div class="browser" class:hidden={!visible}>
  <div class="bar">
    <button class="icon" title="Back" aria-label="Back" onclick={() => call(() => api.browserHistory(tab.id, 'back'))}>
      <svg viewBox="0 0 20 20"><path d="M16 10H4M9 5l-5 5 5 5" /></svg>
    </button>
    <button
      class="icon"
      title="Forward"
      aria-label="Forward"
      onclick={() => call(() => api.browserHistory(tab.id, 'forward'))}
    >
      <svg viewBox="0 0 20 20"><path d="M4 10h12M11 5l5 5-5 5" /></svg>
    </button>
    <button
      class="icon"
      title="Refresh"
      aria-label="Refresh"
      onclick={() => call(() => api.browserHistory(tab.id, 'reload'))}
    >
      <svg viewBox="0 0 20 20"><path d="M16 10a6 6 0 1 1-1.76-4.24M16 3.5v3.5h-3.5" /></svg>
    </button>
    {#if tab.account === null}
      <span class="anonymous muted" title="Signed in as nobody; the profile is discarded when the tab closes"
        >Anonymous</span
      >
    {:else}
      <span class="swatch" style:background={tab.color ?? 'transparent'} title={tab.account}></span>
    {/if}
    <input
      bind:this={address}
      bind:value={url}
      spellcheck="false"
      autocomplete="off"
      onkeydown={(e) => {
        if (e.key === 'Enter') {
          address.blur()
          void call(() => api.browserNavigate(tab.id, url))
        }
      }}
      onblur={() => (url = tab.url)}
    />
    {#if tab.can_fill_email}
      <button class="chip" disabled={busy} title="Fill the username and remember this login page" onclick={fillEmail}>
        Fill {tab.username}
      </button>
    {/if}
    {#if needsPassword}
      <button class="chip" disabled={busy} title="Store a password for {tab.account}, then fill the form" onclick={() => (settingPassword = true)}>
        Set password
      </button>
    {:else if tab.can_sign_in}
      <button class="chip" disabled={busy} title="Fill the login form (Ctrl+Shift+L in the page)" onclick={signIn}>
        Sign in as {tab.account}
      </button>
    {/if}
    {#if tab.account !== null}
      <button disabled={busy} title="Clear this account's session cookies and reload" onclick={endSession}>
        End session
      </button>
    {/if}
  </div>
  {#if tab.status}
    <pre class="status" class:starting={tab.status.endsWith('…')}>{tab.status}</pre>
  {/if}
  <div class="body" class:mobile={tab.mobile}>
    <div class="host" bind:this={host}></div>
    {#if tab.paused}
      <div class="paused">
        <p><strong>{tab.paused.service}</strong> isn't running, so nothing is listening on port {tab.paused.port}.</p>
        <p class="muted">{tab.paused.url}</p>
        <div class="actions">
          <button class="chip" disabled={busy} onclick={startService}>Start {tab.paused.service}</button>
          <button disabled={busy} title="Load the page if the server has been started another way" onclick={retry}>
            Retry
          </button>
        </div>
      </div>
    {/if}
  </div>
</div>

{#if settingPassword && tab.account}
  <PasswordDialog account={tab.account} onSave={savePassword} onClose={() => (settingPassword = false)} />
{/if}

<style>
  .browser {
    position: absolute;
    inset: 0;
    display: flex;
    flex-direction: column;
  }
  .hidden {
    display: none;
  }
  .bar {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 4px 8px;
    border-bottom: 1px solid var(--border);
    background: var(--panel);
  }
  .status {
    margin: 0;
    max-height: 40%;
    overflow: auto;
    padding: 6px 12px;
    font-family: var(--mono);
    font-size: 12px;
    white-space: pre-wrap;
    color: var(--bad);
    background: var(--panel);
    border-bottom: 1px solid var(--border);
  }
  .status.starting {
    color: var(--muted);
  }
  .bar input {
    flex: 1;
    min-width: 0;
    font: inherit;
    font-size: 13px;
    color: var(--text);
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 16px;
    padding: 4px 12px;
  }
  .icon {
    width: 32px;
    height: 32px;
    padding: 0;
    border: 0;
    border-radius: 50%;
    background: none;
    display: grid;
    place-items: center;
    flex: none;
  }
  .icon:hover {
    background: var(--panel-2);
  }
  .icon svg {
    width: 20px;
    height: 20px;
    fill: none;
    stroke: currentColor;
    stroke-width: 1.5;
    stroke-linecap: round;
    stroke-linejoin: round;
  }
  .swatch {
    width: 10px;
    height: 10px;
    border-radius: 50%;
    border: 1px solid var(--border);
    flex: none;
  }
  .anonymous {
    flex: none;
    font-size: 12px;
  }
  .chip {
    border-radius: 16px;
    background: var(--accent);
    color: var(--accent-text);
    border: 0;
    padding: 4px 12px;
    white-space: nowrap;
  }
  .paused {
    position: absolute;
    inset: 0;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 4px;
    padding: 16px;
    text-align: center;
    background: var(--bg);
  }
  .paused p {
    margin: 0;
  }
  .paused .muted {
    color: var(--muted);
    font-size: 12px;
    overflow-wrap: anywhere;
  }
  .paused .actions {
    display: flex;
    gap: 8px;
    margin-top: 12px;
  }
  .body {
    flex: 1;
    min-height: 0;
    display: flex;
    position: relative;
  }
  .host {
    flex: 1;
    min-width: 0;
  }
  .body.mobile {
    justify-content: center;
    align-items: center;
    padding: 16px;
    background: var(--panel-2);
  }
  .body.mobile .host {
    flex: none;
    height: min(852px, 100%);
    aspect-ratio: 393 / 852;
    max-width: 100%;
    border-radius: 12px;
    box-shadow: 0 0 0 8px var(--term-bg);
  }
</style>
