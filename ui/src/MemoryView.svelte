<script lang="ts">
  import { untrack } from 'svelte'
  import {
    api,
    MEMORY_CHANGED_SINCE_EDITING,
    OS_LABEL,
    SESSION_GUIDANCE_LIMIT,
    type Os,
    type ProjectMemory,
    type SessionGuidanceSize,
  } from './api'
  import GuidanceEditor from './GuidanceEditor.svelte'

  interface Props {
    project: string
    visible: boolean
    onError: (message: string) => void
    onEditingChange?: (editing: boolean) => void
  }

  let { project, visible, onError, onEditingChange }: Props = $props()

  const FILES = [
    { key: 'shared', label: 'Shared' },
    ...(['windows', 'macos', 'linux'] as const).map((os) => ({ key: os, label: OS_LABEL[os] })),
  ]

  let memories = $state<ProjectMemory[]>([])
  let selected = $state('shared')
  let sessionSize = $state<(SessionGuidanceSize & { project: string }) | null>(null)

  const os = $derived(selected === 'shared' ? null : (selected as Os))
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

  async function save(base: string, text: string) {
    if (current) await api.saveMemory(current.project, current.os, base, text)
  }
</script>

{#snippet intro()}
  {#if current}
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
  {/if}
{/snippet}

<GuidanceEditor
  title="Shared memory"
  files={FILES}
  bind:selected
  {current}
  changedMarker={MEMORY_CHANGED_SINCE_EDITING}
  refusedMessage="The memory changed since you started editing, probably from a change you approved."
  empty="_Nothing yet. Claudes will propose facts as they work._"
  {intro}
  {save}
  reload={refresh}
  {onError}
  onUnsavedChange={onEditingChange}
/>

<style>
  .warn {
    color: var(--warn);
  }
</style>
