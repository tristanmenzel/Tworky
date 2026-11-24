<script lang="ts">
  import { onMount, untrack } from 'svelte'
  import { listen } from '@tauri-apps/api/event'
  import {
    api,
    GUIDANCE_CHANGED_SINCE_EDITING,
    OS_LABEL,
    SESSION_GUIDANCE_LIMIT,
    guidanceLimitNote,
    type GuidanceFile,
    type Os,
    type SessionGuidanceSize,
  } from './api'
  import GuidanceEditor from './GuidanceEditor.svelte'

  interface Props {
    visible: boolean
    onError: (message: string) => void
    onUnsavedChange: (unsaved: boolean) => void
  }

  let { visible, onError, onUnsavedChange }: Props = $props()

  const FILES = [
    { key: 'shared', label: 'Shared' },
    ...(['windows', 'macos', 'linux'] as const).map((os) => ({ key: os, label: OS_LABEL[os] })),
  ]

  let files = $state<GuidanceFile[]>([])
  let selected = $state('shared')
  let sessionSize = $state<SessionGuidanceSize | null>(null)
  let editor: GuidanceEditor | undefined = $state()

  const os = $derived(selected === 'shared' ? null : (selected as Os))
  const current = $derived(files.find((f) => f.os === os))

  async function load() {
    try {
      ;[files, sessionSize] = await Promise.all([api.guidance(), api.sessionGuidanceSize(null)])
    } catch (e) {
      onError(String(e))
    }
  }

  $effect(() => {
    if (visible) untrack(load)
  })

  onMount(() => {
    const unlisten = listen('memory-changes', load)
    return () => void unlisten.then((f) => f())
  })

  export function discard() {
    editor?.discard()
  }

  async function save(base: string, text: string) {
    await api.saveGuidance(os, base, text)
  }
</script>

{#snippet intro()}
  {#if current}
    {#if current.os}
      Every Claude session Tworky starts on {OS_LABEL[current.os]} sees this after the shared guidance, and it takes precedence
      where they conflict.
    {:else}
      Every Claude session Tworky starts sees this after Tworky's own guidance and before a project's memory, when it
      starts, resumes, clears or compacts. Where it conflicts with Tworky's guidance, it takes precedence.
    {/if}
    Claudes don't propose changes to it; only you edit it, here. Saving commits it to your config repo, which syncs it to
    your other machines. Stored in
    <code>{current.path}</code>.
    {#if sessionSize && (sessionSize.bytes > SESSION_GUIDANCE_LIMIT || sessionSize.measured > sessionSize.budget)}<span
        class="warn"
        >Sessions on {OS_LABEL[sessionSize.os]}{sessionSize.project ? ` for ${sessionSize.project}` : ''} load {Math.ceil(
          sessionSize.bytes / 1024,
        )} KB of guidance and memory. {guidanceLimitNote(sessionSize)}</span
      >{/if}
  {/if}
{/snippet}

<GuidanceEditor
  bind:this={editor}
  title="Guidance"
  files={FILES}
  bind:selected
  {current}
  changedMarker={GUIDANCE_CHANGED_SINCE_EDITING}
  refusedMessage="The guidance changed since you started editing, probably from a sync."
  empty="_Nothing yet. Add guidance every session should follow._"
  {intro}
  {save}
  reload={load}
  {onError}
  {onUnsavedChange}
/>

<style>
  .warn {
    color: var(--warn);
  }
</style>
