<script lang="ts">
  import { listen } from '@tauri-apps/api/event'
  import { untrack } from 'svelte'
  import { api, type Account, type AccountsPayload, type Service } from './api'
  import { ask } from './confirm.svelte'

  interface Props {
    project: string
    services: Service[]
    visible: boolean
    onChanged: () => void
    onError: (message: string) => void
  }

  let { project, services, visible, onChanged, onError }: Props = $props()

  interface Draft {
    previous: string | null
    name: string
    username: string
    start: 'url' | 'service'
    start_url: string
    start_service: string
    mobile: boolean
    origins: string
    color: string
  }

  let payload = $state<AccountsPayload | null>(null)
  let draft = $state<Draft | null>(null)
  let passwordFor = $state<string | null>(null)
  let password = $state('')
  let busy = $state(false)
  let servedServices = $derived(services.filter((s) => s.port !== null).map((s) => s.name))

  async function load() {
    try {
      payload = await api.accounts(project)
    } catch (e) {
      onError(String(e))
    }
  }

  $effect(() => {
    void project
    if (visible) untrack(load)
  })

  $effect(() => {
    const unlisten = listen<{ project: string; account: string; origin: string }>('accounts-changed', (e) => {
      if (e.payload.project !== project) return
      const d = draft
      if (d?.previous === e.payload.account) {
        const origins = d.origins
          .split('\n')
          .map((o) => o.trim())
          .filter(Boolean)
        if (!origins.includes(e.payload.origin)) d.origins = [...origins, e.payload.origin].join('\n')
      }
      void load()
    })
    return () => void unlisten.then((f) => f())
  })

  async function act(work: () => Promise<unknown>) {
    busy = true
    try {
      await work()
      await load()
      onChanged()
      return true
    } catch (e) {
      onError(String(e))
      return false
    } finally {
      busy = false
    }
  }

  function edit(a: Account | null) {
    passwordFor = null
    draft = {
      previous: a?.name ?? null,
      name: a?.name ?? '',
      username: a?.username ?? '',
      start: a?.start_service ? 'service' : 'url',
      start_url: a?.start_url || 'http://localhost:',
      start_service: a?.start_service ?? servedServices[0] ?? '',
      mobile: a?.mobile ?? false,
      origins: a?.login_origins.join('\n') ?? '',
      color: a?.color ?? '',
    }
  }

  async function save() {
    if (!draft) return
    const d = draft
    const account: Account = {
      name: d.name.trim(),
      username: d.username.trim(),
      start_url: d.start === 'url' ? d.start_url.trim() : '',
      start_service: d.start === 'service' ? d.start_service : null,
      mobile: d.mobile,
      login_origins: d.origins
        .split('\n')
        .map((o) => o.trim())
        .filter(Boolean),
      color: d.color || null,
    }
    if (await act(() => api.saveAccount(project, d.previous, account))) draft = null
  }

  async function savePassword() {
    if (!passwordFor || !password) return
    const name = passwordFor
    if (await act(() => api.setAccountPassword(project, name, password))) {
      passwordFor = null
      password = ''
    }
  }

  async function remove(name: string) {
    const ok = await ask({
      title: `Remove ${name}?`,
      message: 'Its stored password is deleted too.',
      confirmLabel: 'Remove account',
      danger: true,
    })
    if (ok) void act(() => api.removeAccount(project, name))
  }
</script>

<div class="accounts">
  <header>
    <h1>Accounts</h1>
    <span class="spacer"></span>
    <button class="primary" onclick={() => edit(null)} disabled={busy || draft?.previous === null}>Add account</button>
  </header>

  <p class="muted small">
    The personas a session's browser can sign in as. Passwords are kept in the OS credential store and never shown to
    Claude. Write URLs with the main checkout's ports; each worker gets its own.
  </p>

  {#snippet form(d: Draft)}
    <form
      class="card form"
      onsubmit={(e) => {
        e.preventDefault()
        void save()
      }}
    >
      <label>Name <input bind:value={d.name} placeholder="Tester 1" required /></label>
      <label>Username <input bind:value={d.username} autocomplete="off" spellcheck="false" /></label>
      <label>
        Start page
        <span class="row">
          <select bind:value={d.start}>
            <option value="url">URL</option>
            <option value="service" disabled={!servedServices.length && d.start !== 'service'}>Service</option>
          </select>
          {#if d.start === 'url'}
            <input class="grow" bind:value={d.start_url} spellcheck="false" required />
          {:else}
            <select class="grow" bind:value={d.start_service} required>
              {#each servedServices as name (name)}
                <option value={name}>{name}</option>
              {/each}
              {#if d.start_service && !servedServices.includes(d.start_service)}
                <option value={d.start_service}>{d.start_service} (missing or no port)</option>
              {/if}
            </select>
          {/if}
        </span>
      </label>
      <label>
        Login origins
        <textarea
          bind:value={d.origins}
          rows="2"
          spellcheck="false"
          placeholder="https://login.example.com/ or service:<name> (one per line)"
        ></textarea>
      </label>
      <div class="row">
        <label class="inline"><input type="checkbox" bind:checked={d.mobile} /> Mobile</label>
        <label class="inline">
          Colour <input type="color" value={d.color || '#888888'} oninput={(e) => (d.color = e.currentTarget.value)} />
        </label>
        {#if d.color}<button type="button" class="link" onclick={() => (d.color = '')}>No colour</button>{/if}
        <span class="spacer"></span>
        <button type="button" onclick={() => (draft = null)} disabled={busy}>Cancel</button>
        <button type="submit" class="primary" disabled={busy}>Save</button>
      </div>
    </form>
  {/snippet}

  {#if draft && draft.previous === null}
    {@render form(draft)}
  {/if}

  {#if payload}
    {#each payload.accounts as a (a.name)}
      {#if draft && draft.previous === a.name}
        {@render form(draft)}
      {:else}
        <section class="card">
          <div class="row">
            <span class="swatch" style:background={a.color ?? 'transparent'}></span>
            <strong>{a.name}</strong>
            {#if a.mobile}<span class="badge">mobile</span>{/if}
            <span class="spacer"></span>
            <button onclick={() => edit(a)} disabled={busy}>Edit</button>
            <button
              onclick={() => {
                draft = null
                password = ''
                passwordFor = a.name
              }}
              disabled={busy}>{a.has_password ? 'Change password' : 'Set password'}</button
            >
            <button class="danger" onclick={() => remove(a.name)} disabled={busy}>Remove</button>
          </div>
          <dl>
            <dt>Username</dt>
            <dd>{a.username || '—'}</dd>
            <dt>Start page</dt>
            <dd>
              {#if a.start_service}service <code>{a.start_service}</code>{:else}<code>{a.start_url}</code>{/if}
            </dd>
            {#if a.login_origins.length}
              <dt>Login origins</dt>
              <dd>{#each a.login_origins as o (o)}<code>{o}</code> {/each}</dd>
            {/if}
            <dt>Password</dt>
            <dd class:ok={a.has_password} class:warn={!a.has_password}>{a.has_password ? 'stored ✓' : 'not set'}</dd>
          </dl>
          {#if passwordFor === a.name}
            <form
              class="row"
              onsubmit={(e) => {
                e.preventDefault()
                void savePassword()
              }}
            >
              <!-- svelte-ignore a11y_autofocus -->
              <input type="password" bind:value={password} autocomplete="new-password" placeholder="Password" autofocus />
              <button type="button" onclick={() => (passwordFor = null)} disabled={busy}>Cancel</button>
              <button type="submit" class="primary" disabled={busy || !password}>Store</button>
            </form>
          {/if}
        </section>
      {/if}
    {:else}
      {#if !draft}
        <p class="muted">No accounts yet.</p>
      {/if}
    {/each}
  {:else}
    <p class="muted">Loading…</p>
  {/if}
</div>

<style>
  .accounts {
    padding: 16px 24px 32px;
    max-width: 980px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  header,
  .row {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  h1 {
    font-size: 18px;
    font-weight: 600;
    margin: 0;
  }
  .spacer {
    flex: 1;
  }
  .small {
    font-size: 12px;
    margin: 0;
  }
  .card {
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 10px 14px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .swatch {
    width: 10px;
    height: 10px;
    border-radius: 50%;
    border: 1px solid var(--border);
    flex: none;
  }
  .badge {
    font-size: 11px;
    padding: 0 6px;
    border-radius: 8px;
    background: var(--panel-2);
    color: var(--muted);
  }
  dl {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 2px 12px;
    margin: 0;
    font-size: 13px;
  }
  dt {
    color: var(--muted);
  }
  dd {
    margin: 0;
    overflow-wrap: anywhere;
  }
  .ok {
    color: var(--ok);
  }
  .warn {
    color: var(--warn);
  }
  .form label {
    display: flex;
    flex-direction: column;
    gap: 2px;
    font-size: 12px;
    color: var(--muted);
  }
  .form label.inline {
    flex-direction: row;
    align-items: center;
    gap: 6px;
  }
  .form input:not([type='checkbox']):not([type='color']),
  .form select,
  .form textarea,
  .row input[type='password'] {
    font: inherit;
    font-size: 13px;
    color: var(--text);
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 4px 8px;
  }
  .row input[type='password'],
  .grow {
    flex: 1;
  }
  .form textarea {
    font-family: var(--mono);
    font-size: 12px;
    resize: vertical;
  }
</style>
