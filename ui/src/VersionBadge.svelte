<script lang="ts">
  import { onMount, untrack } from 'svelte'
  import { api, type UpdateProgress, type VersionInfo } from './api'
  import { confirmOverBrowser } from './overlay.svelte'

  let { liveSessions, refreshKey }: { liveSessions: number; refreshKey: number } = $props()

  const CHECK_MS = 60 * 60 * 1000
  const PROGRESS_MS = 1000

  let info = $state<VersionInfo>()
  let report = $state<UpdateProgress>()
  let startedHere = $state(false)
  let starting = $state(false)
  let startError = $state('')
  let checking = $state(false)
  let lastChecked = $state<Date>()
  let failedCheck = $state<{ at: Date; error: string }>()
  let progressTimer: ReturnType<typeof setInterval> | undefined

  const phase = $derived(report?.progress?.phase)
  const failed = $derived(phase === 'failed' || !!startError)
  const inFlight = $derived(startedHere && (phase === 'downloading' || phase === 'installing' || (starting && !phase)))
  const ready = $derived(startedHere && phase === 'ready')

  async function check() {
    if (checking) return
    checking = true
    try {
      const next = await api.updateStatus()
      if (next.error && !next.status) {
        failedCheck = { at: new Date(), error: next.error }
        if (!info?.status) info = next
      } else {
        info = next
        lastChecked = new Date()
        failedCheck = undefined
      }
    } catch (e) {
      failedCheck = { at: new Date(), error: String(e) }
    } finally {
      checking = false
    }
  }

  async function poll() {
    try {
      report = await api.updateProgress()
    } catch {
      return
    }
    if (phase !== 'downloading' && phase !== 'installing') stopPolling()
  }

  function stopPolling() {
    clearInterval(progressTimer)
    progressTimer = undefined
  }

  async function update() {
    startError = ''
    if (phase === 'failed') await api.dismissUpdateFailure()
    report = undefined
    starting = true
    startedHere = true
    try {
      await api.startUpdate()
      progressTimer ??= setInterval(poll, PROGRESS_MS)
    } catch (e) {
      startError = String(e)
      startedHere = false
    } finally {
      starting = false
    }
  }

  async function restart() {
    if (liveSessions > 0) {
      const sessions = liveSessions === 1 ? '1 running session' : `${liveSessions} running sessions`
      if (!(await confirmOverBrowser(`Restart to update? ${sessions} will be stopped.`))) return
    }
    await api.restartForUpdate()
  }

  $effect(() => {
    void refreshKey
    untrack(check)
  })

  onMount(() => {
    api.updateProgress().then((r) => {
      if (r.progress?.phase === 'failed') report = r
      else if (r.progress?.phase === 'done') api.dismissUpdateFailure()
    })
    const timer = setInterval(() => document.hidden || check(), CHECK_MS)
    return () => {
      clearInterval(timer)
      stopPolling()
    }
  })

  const checkedLine = $derived.by(() => {
    const lines = [
      lastChecked && `Last checked ${lastChecked.toLocaleString()}`,
      failedCheck && `Last check failed at ${failedCheck.at.toLocaleString()}: ${failedCheck.error}`,
    ].filter(Boolean)
    return lines.length ? `\n\n${lines.join('\n')}` : ''
  })

  const failureDetail = $derived(startError || [report?.progress?.message, report?.log_tail].filter(Boolean).join('\n\n'))
</script>

{#if info && !info.dev}
  <span class="version">
    {#if ready}
      <button
        class="action"
        onclick={restart}
        title={`The update is downloaded. Tworky closes, installs it and reopens${checkedLine}`}>Restart to update</button
      >
    {:else if inFlight}
      <span class="pill warn" title={`${report?.log_tail || 'Starting the update'}${checkedLine}`}>
        {phase === 'installing' ? 'Installing update…' : 'Downloading update…'}
      </span>
    {:else if failed}
      <button class="pill warn" onclick={update} title={`${failureDetail}\n\nClick to try again${checkedLine}`}
        >Update failed</button
      >
    {:else if info.status?.kind === 'latest'}
      <span class="pill ok" title={`Tworky ${info.version}, built from ${info.commit}${checkedLine}`}>latest</span>
    {:else if info.status?.kind === 'behind'}
      <span class="pill warn" title={`Tworky ${info.version}, built from ${info.commit}; the latest release is ${info.status.tag}${checkedLine}`}>
        {info.status.commits === 1 ? '1 commit behind' : `${info.status.commits} commits behind`}
      </span>
      <button class="action" onclick={update} title="Download the latest release">Update</button>
    {:else}
      <span
        class="pill muted"
        title={`Tworky ${info.version}, built from ${info.commit}${info.status?.kind === 'unknown' ? `: ${info.status.reason}` : info.error ? `\n${info.error}` : ''}${checkedLine}`}
        >{info.short}</span
      >
    {/if}
    {#if !inFlight}
      <button
        class="refresh"
        class:spinning={checking}
        onclick={check}
        disabled={checking}
        aria-label="Check for updates"
        title={`Check for updates${checkedLine}`}><span>↻</span></button
      >
    {/if}
  </span>
{/if}

<style>
  .version {
    display: inline-flex;
    align-items: center;
    gap: 6px;
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
  }
  .pill.ok {
    color: var(--ok);
    border-color: color-mix(in srgb, var(--ok) 55%, transparent);
  }
  .pill.warn {
    color: var(--warn);
    border-color: color-mix(in srgb, var(--warn) 55%, transparent);
  }
  .pill.muted {
    color: var(--muted);
    border-color: var(--border);
    font-family: var(--mono);
    font-weight: 400;
  }
  button.pill {
    cursor: pointer;
  }
  .action {
    font-size: 12px;
    padding: 1px 10px;
  }
  .refresh {
    font-size: 13px;
    line-height: 1;
    padding: 2px 4px;
    border: 0;
    background: none;
    color: var(--muted);
  }
  .refresh:hover:not(:disabled) {
    color: inherit;
  }
  .refresh:disabled {
    opacity: 1;
  }
  .refresh span {
    display: inline-block;
  }
  .refresh.spinning span {
    animation: spin 0.8s linear infinite;
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
</style>
