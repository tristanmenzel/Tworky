<script lang="ts">
  import { onMount } from 'svelte'
  import { Channel } from '@tauri-apps/api/core'
  import { openUrl } from '@tauri-apps/plugin-opener'
  import { Terminal } from '@xterm/xterm'
  import { FitAddon } from '@xterm/addon-fit'
  import { WebLinksAddon } from '@xterm/addon-web-links'
  import { api } from './api'
  import { registerTerminal } from './terminals'

  interface Props {
    workerId: string
    visible: boolean
    generation: number
    onError: (message: string) => void
  }

  let { workerId, visible, generation, onError }: Props = $props()

  let host: HTMLDivElement
  let term: Terminal | undefined
  let fit: FitAddon | undefined
  let unregister: (() => void) | undefined

  const paste = (text: string) => term?.paste(text)

  function css(name: string): string {
    return getComputedStyle(document.documentElement).getPropertyValue(name).trim()
  }

  function resize() {
    if (!term || !fit || !visible || host.clientWidth === 0) return
    fit.fit()
    api.resize(workerId, term.cols, term.rows).catch(() => {})
  }

  const isMac = navigator.userAgent.includes('Mac')
  const isWindows = navigator.userAgent.includes('Windows')

  /**
   * xterm.js sends Ctrl+C and Ctrl+V to the pty as ^C and ^V. Returning false for a paste lets the
   * browser fire its paste event, which xterm turns into input (bracketed when the app asks for it).
   * Shift+Enter sends Ctrl+J, which Claude Code reads as a newline in every terminal.
   */
  function customKeys(e: KeyboardEvent): boolean {
    if (e.key === 'Enter' && e.shiftKey && !e.ctrlKey && !e.altKey && !e.metaKey && !e.isComposing) {
      if (e.type === 'keydown') {
        e.preventDefault()
        api.write(workerId, '\n').catch(() => {})
      }
      return false
    }
    if (isMac || !e.ctrlKey || e.altKey || e.metaKey) return true
    const key = e.key.toLowerCase()
    if (key === 'v') return false
    if (key === 'c' && (e.shiftKey || term?.hasSelection())) {
      if (e.type === 'keydown' && term?.hasSelection()) {
        navigator.clipboard.writeText(term.getSelection()).catch((err) => onError(`Copy failed: ${err}`))
        term.clearSelection()
      }
      e.preventDefault()
      return false
    }
    return true
  }

  /**
   * xterm.js only pastes text, so an image-only paste instead sends Claude Code's own image-paste key,
   * and Claude Code reads the image from the OS clipboard itself.
   */
  function pasteImage(e: ClipboardEvent) {
    const data = e.clipboardData
    if (!data || data.getData('text/plain')) return
    const hasImage = Array.from(data.items).some((item) => item.kind === 'file' && item.type.startsWith('image/'))
    if (!hasImage) return
    e.preventDefault()
    e.stopPropagation()
    api.write(workerId, isWindows ? '\x1bv' : '\x16').catch(() => {})
  }

  async function connect() {
    if (!term) return
    unregister?.()
    unregister = undefined
    term.reset()
    const channel = new Channel<string>()
    channel.onmessage = (data) => term?.write(data)
    try {
      fit?.fit()
      await api.openSession(workerId, term.cols, term.rows, channel)
      if (term) unregister = registerTerminal(workerId, paste)
    } catch (e) {
      onError(String(e))
    }
  }

  onMount(() => {
    term = new Terminal({
      fontFamily: css('--mono') || 'Consolas, monospace',
      fontSize: 13,
      cursorBlink: true,
      allowProposedApi: true,
      scrollback: 10000,
      theme: { background: css('--term-bg') || '#0d0f12' },
    })
    fit = new FitAddon()
    term.loadAddon(fit)
    term.loadAddon(new WebLinksAddon((_e, uri) => openUrl(uri)))
    term.open(host)
    term.attachCustomKeyEventHandler(customKeys)
    term.onData((data) => api.write(workerId, data).catch(() => {}))
    host.addEventListener('paste', pasteImage, true)
    const observer = new ResizeObserver(() => resize())
    observer.observe(host)
    return () => {
      observer.disconnect()
      unregister?.()
      host.removeEventListener('paste', pasteImage, true)
      term?.dispose()
      term = undefined
    }
  })

  $effect(() => {
    void generation
    connect()
  })

  $effect(() => {
    if (visible) {
      requestAnimationFrame(() => {
        resize()
        term?.focus()
      })
    }
  })
</script>

<div class="term" class:hidden={!visible}>
  <div class="host" bind:this={host}></div>
</div>

<style>
  .term {
    position: absolute;
    inset: 0;
    padding: 6px 0 0 8px;
    background: var(--term-bg);
  }
  .host {
    width: 100%;
    height: 100%;
  }
  .hidden {
    visibility: hidden;
    pointer-events: none;
  }
</style>
