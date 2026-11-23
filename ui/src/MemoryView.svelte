<script lang="ts">
  import { onDestroy, untrack } from 'svelte'
  import {
    api,
    MEMORY_CHANGED_SINCE_EDITING,
    OS_LABEL,
    type Os,
    type ProjectMemory,
    type SessionGuidanceSize,
  } from './api'
  import Markdown from './Markdown.svelte'

  interface Props {
    project: string
    visible: boolean
    onError: (message: string) => void
    onEditingChange?: (editing: boolean) => void
  }

  let { project, visible, onError, onEditingChange }: Props = $props()

  const FILES: { os: Os | null; label: string }[] = [
    { os: null, label: 'Shared' },
    ...(['windows', 'macos', 'linux'] as const).map((os) => ({ os, label: OS_LABEL[os] })),
  ]

  let memories = $state<ProjectMemory[]>([])
  let os = $state<Os | null>(null)
  let editing = $state(false)
  let draft = $state('')
  let base = ''
  let refused = $state(false)
  let saving = $state(false)
  let sessionSize = $state<(SessionGuidanceSize & { project: string }) | null>(null)

  const SESSION_GUIDANCE_LIMIT = 8192

  const current = $derived(memories.find((m) => m.project === project && m.os === os))

  async function load() {
    try {
      memories = await api.memories()
    } catch (e) {
      onError(String(e))
    }
  }

  async function loadSessionSize(name: string) {
    try {
      const size = await api.sessionGuidanceSize(name)
      if (name === project) sessionSize = { ...size, project: name }
    } catch (e) {
      onError(String(e))
    }
  }

  async function refresh() {
    await Promise.all([load(), loadSessionSize(project)])
  }

  $effect(() => {
    if (visible) untrack(load)
  })

  $effect(() => {
    const name = project
    if (visible) untrack(() => loadSessionSize(name))
  })

  $effect(() => {
    onEditingChange?.(editing)
  })

  onDestroy(() => onEditingChange?.(false))

  function edit() {
    draft = current?.text ?? ''
    base = current?.hash ?? ''
    refused = false
    editing = true
  }

  function cancel() {
    editing = false
    refused = false
  }

  async function reload() {
    cancel()
    await refresh()
  }

  async function save() {
    if (!current) return
    saving = true
    try {
      await api.saveMemory(current.project, current.os, base, draft)
      editing = false
      await refresh()
    } catch (e) {
      if (String(e).includes(MEMORY_CHANGED_SINCE_EDITING)) refused = true
      else onError(String(e))
    } finally {
      saving = false
    }
  }
</script>

<div class="memory">
  <header>
    <h1>Shared memory</h1>
    <span class="spacer"></span>
    {#if editing}
      <button onclick={cancel} disabled={saving}>Cancel</button>
      <button class="primary" onclick={save} disabled={saving}>{saving ? 'Saving…' : 'Save'}</button>
    {:else if current}
      <button onclick={refresh}>Reload</button>
      <button class="primary" onclick={edit}>Edit</button>
    {/if}
  </header>

  <nav class="files" aria-label="Memory file">
    {#each FILES as file (file.label)}
      <button class:active={file.os === os} disabled={editing && file.os !== os} onclick={() => (os = file.os)}
        >{file.label}</button
      >
    {/each}
  </nav>

  {#if current}
    <p class="muted small">
      {#if current.os}
        Claudes working on {current.project} on {OS_LABEL[current.os]} see this after the shared memory, and it takes precedence
        where they conflict. Sessions propose changes to it with <code>--os {current.os}</code>.
      {:else}
        Every Claude working on {current.project} sees this when its session starts, resumes, clears or compacts, and proposes
        changes for you to approve in the Changes tab.
      {/if}
      Stored in <code>{current.path}</code>.
      {#if sessionSize?.project === current.project && sessionSize.bytes > SESSION_GUIDANCE_LIMIT}<span class="warn"
          >Sessions on {OS_LABEL[sessionSize.os]} load {Math.ceil(sessionSize.bytes / 1024)} KB of guidance and memory, over
          8 KB, so they get the file paths to read instead of the text. Trim it.</span
        >{/if}
    </p>
    {#if editing}
      {#if refused}
        <div class="refused" role="alert">
          <span
            >The memory changed since you started editing, probably from a change you approved. Your draft is still
            below. Copy anything you need, then reload.</span
          >
          <button onclick={reload}>Reload</button>
        </div>
      {/if}
      <textarea bind:value={draft} spellcheck="false"></textarea>
    {:else}
      <section class="body">
        <Markdown source={current.text} empty="_Nothing yet. Claudes will propose facts as they work._" />
      </section>
    {/if}
  {:else}
    <p class="muted">Loading…</p>
  {/if}
</div>

<style>
  .memory {
    padding: 16px 24px 32px;
    max-width: 980px;
    height: 100%;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  header {
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
  .files {
    display: flex;
    gap: 2px;
    border-bottom: 1px solid var(--border);
  }
  .files button {
    border-radius: 6px 6px 0 0;
    border: 1px solid transparent;
    border-bottom: 0;
    background: none;
  }
  .files button.active {
    background: var(--panel);
    border-color: var(--border);
  }
  .small {
    font-size: 12px;
    margin: 0;
  }
  .warn {
    color: var(--warn);
  }
  .body {
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 12px 16px;
  }
  .refused {
    display: flex;
    gap: 10px;
    align-items: center;
    flex-wrap: wrap;
    padding: 8px 12px;
    border: 1px solid var(--warn);
    border-radius: 8px;
    background: var(--panel);
  }
  .refused span {
    flex: 1;
    min-width: 0;
    color: var(--warn);
  }
  textarea {
    flex: 1;
    min-height: 320px;
    font-family: var(--mono);
    font-size: 12px;
    color: var(--text);
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 10px 12px;
    resize: none;
  }
</style>
