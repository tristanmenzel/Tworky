<script lang="ts">
  import { onDestroy, untrack } from 'svelte'
  import { api, MEMORY_CHANGED_SINCE_EDITING, type ProjectMemory } from './api'
  import Markdown from './Markdown.svelte'

  interface Props {
    project: string
    visible: boolean
    onError: (message: string) => void
    onEditingChange?: (editing: boolean) => void
  }

  let { project, visible, onError, onEditingChange }: Props = $props()

  let memories = $state<ProjectMemory[]>([])
  let editing = $state(false)
  let draft = $state('')
  let base = ''
  let refused = $state(false)
  let saving = $state(false)

  const current = $derived(memories.find((m) => m.project === project))

  async function load() {
    try {
      memories = await api.memories()
    } catch (e) {
      onError(String(e))
    }
  }

  $effect(() => {
    if (visible) untrack(load)
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
    await load()
  }

  async function save() {
    if (!current) return
    saving = true
    try {
      await api.saveMemory(current.project, base, draft)
      editing = false
      await load()
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
      <button onclick={load}>Reload</button>
      <button class="primary" onclick={edit}>Edit</button>
    {/if}
  </header>

  {#if current}
    <p class="muted small">
      Every Claude working on {current.project} sees this when its session starts or resumes, and proposes changes for you
      to approve in the Changes tab. Stored in <code>{current.path}</code>.
      {#if current.text.length > 8192}<span class="warn">Over 8 KB: sessions may see it truncated, so trim it.</span
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
