<script lang="ts">
  import { onDestroy, type Snippet } from 'svelte'
  import { ask } from './confirm.svelte'
  import Markdown from './Markdown.svelte'

  interface Props {
    title: string
    files: { key: string; label: string }[]
    selected: string
    /** The selected file as last loaded. */
    current: { text: string; hash: string } | undefined
    /** Text of the error a save gets when the file changed since `base`. */
    changedMarker: string
    refusedMessage: string
    empty: string
    intro: Snippet
    save: (base: string, text: string) => Promise<void>
    reload: () => Promise<void>
    onError: (message: string) => void
    onUnsavedChange?: (unsaved: boolean) => void
  }

  let {
    title,
    files,
    selected = $bindable(),
    current,
    changedMarker,
    refusedMessage,
    empty,
    intro,
    save,
    reload,
    onError,
    onUnsavedChange,
  }: Props = $props()

  let editing = $state(false)
  let draft = $state('')
  let original = $state('')
  let base = ''
  let refused = $state(false)
  let saving = $state(false)

  const unsaved = $derived(editing && draft !== original)

  $effect(() => {
    onUnsavedChange?.(unsaved)
  })

  onDestroy(() => onUnsavedChange?.(false))

  function edit() {
    draft = current?.text ?? ''
    original = draft
    base = current?.hash ?? ''
    refused = false
    editing = true
  }

  export function discard() {
    editing = false
    refused = false
  }

  async function pick(key: string) {
    if (key === selected) return
    if (unsaved && !(await ask({ title: 'Discard your unsaved edit?', confirmLabel: 'Discard', danger: true }))) return
    discard()
    selected = key
  }

  async function reloadAfterRefusal() {
    discard()
    await reload()
  }

  async function onSave() {
    saving = true
    try {
      await save(base, draft)
      editing = false
      await reload()
    } catch (e) {
      if (String(e).includes(changedMarker)) refused = true
      else onError(String(e))
    } finally {
      saving = false
    }
  }
</script>

<div class="editor">
  <header>
    <h1>{title}</h1>
    <span class="spacer"></span>
    {#if editing}
      <button onclick={discard} disabled={saving}>Discard</button>
      <button class="primary" onclick={onSave} disabled={saving}>{saving ? 'Saving…' : 'Save'}</button>
    {:else if current}
      <button onclick={reload}>Reload</button>
      <button class="primary" onclick={edit}>Edit</button>
    {/if}
  </header>

  <nav class="files" aria-label="File">
    {#each files as file (file.key)}
      <button class:active={file.key === selected} onclick={() => pick(file.key)}>{file.label}</button>
    {/each}
  </nav>

  {#if current}
    <p class="muted small">{@render intro()}</p>
    {#if editing}
      {#if refused}
        <div class="refused" role="alert">
          <span>{refusedMessage} Your draft is still below. Copy anything you need, then reload.</span>
          <button onclick={reloadAfterRefusal}>Reload</button>
        </div>
      {/if}
      <textarea bind:value={draft} spellcheck="false"></textarea>
    {:else}
      <section class="body">
        <Markdown source={current.text} {empty} />
      </section>
    {/if}
  {:else}
    <p class="muted">Loading…</p>
  {/if}
</div>

<style>
  .editor {
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
