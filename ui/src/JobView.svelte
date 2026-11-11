<script lang="ts">
  import { onMount } from 'svelte'
  import { Terminal } from '@xterm/xterm'
  import { FitAddon } from '@xterm/addon-fit'
  import { api, type Job, type Worker } from './api'
  import { formatAge, formatBytes } from './issue'

  interface Props {
    worker: Worker
    job: Job
    onError: (message: string) => void
  }

  let { worker, job, onError }: Props = $props()

  const POLL_MS = 1000

  let host = $state<HTMLDivElement>()
  let stopping = $state(false)

  function css(name: string): string {
    return getComputedStyle(document.documentElement).getPropertyValue(name).trim()
  }

  async function stop() {
    stopping = true
    try {
      await api.stopJob(worker.id, job.pid)
    } catch (e) {
      onError(String(e))
      stopping = false
    }
  }

  onMount(() => {
    const output = job.output
    if (!output || !host) return
    const term = new Terminal({
      fontFamily: css('--mono') || 'Consolas, monospace',
      fontSize: 13,
      disableStdin: true,
      convertEol: true,
      scrollback: 10000,
      theme: { background: css('--term-bg') || '#0d0f12' },
    })
    const fit = new FitAddon()
    term.loadAddon(fit)
    term.open(host)
    fit.fit()
    const observer = new ResizeObserver(() => fit.fit())
    observer.observe(host)

    let offset: number | null = null
    let timer: ReturnType<typeof setTimeout> | undefined
    let closed = false
    async function poll() {
      try {
        const chunk = await api.jobOutput(output!, offset)
        if (chunk.text) term.write(chunk.text)
        offset = chunk.offset
      } catch (e) {
        term.write(`\r\n[could not read output: ${e}]\r\n`)
        return
      }
      if (!closed) timer = setTimeout(poll, POLL_MS)
    }
    poll()
    return () => {
      closed = true
      clearTimeout(timer)
      observer.disconnect()
      term.dispose()
    }
  })
</script>

<div class="job">
  <div class="toolbar">
    <code class="command" title={job.command}>{job.command}</code>
    <span class="muted meta">
      up {formatAge(job.age_secs)} · {formatBytes(job.memory_bytes)} · {job.process_count}
      {job.process_count === 1 ? 'process' : 'processes'} · pid {job.pid}
    </span>
    <button class="danger" disabled={stopping} onclick={stop}>{stopping ? 'Stopping…' : 'Stop'}</button>
  </div>
  {#if job.output}
    <div class="term">
      <div class="host" bind:this={host}></div>
    </div>
  {:else}
    <p class="muted pad">
      {job.from_claude
        ? 'No output file: Claude ran this in the foreground, so its output is in the session.'
        : "This wasn't started as one of Claude's background commands, so its output isn't captured."}
    </p>
  {/if}
</div>

<style>
  .job {
    position: absolute;
    inset: 0;
    display: flex;
    flex-direction: column;
  }
  .toolbar {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 6px 10px;
    border-bottom: 1px solid var(--border);
    background: var(--panel);
    min-width: 0;
  }
  .command {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .meta {
    margin-left: auto;
    flex: none;
    white-space: nowrap;
  }
  .toolbar button {
    flex: none;
  }
  .term {
    position: relative;
    flex: 1;
    min-height: 0;
    padding: 6px 0 0 8px;
    background: var(--term-bg);
  }
  .host {
    width: 100%;
    height: 100%;
  }
  .pad {
    padding: 16px 20px;
  }
</style>
