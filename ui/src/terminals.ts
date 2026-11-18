interface TerminalHandle {
  paste: (text: string) => void
  /** True once the program in the terminal reads input, which Claude Code signals by enabling bracketed paste. */
  ready: () => boolean
  focus: () => void
  redraw: () => void
}

const handles = new Map<string, TerminalHandle>()

export function registerTerminal(id: string, handle: TerminalHandle): () => void {
  handles.set(id, handle)
  return () => {
    if (handles.get(id) === handle) handles.delete(id)
  }
}

export function redrawTerminal(id: string) {
  handles.get(id)?.redraw()
}

/**
 * Waits up to `timeoutMs` for the terminal to attach and Claude to take input; false if it doesn't.
 * Doesn't submit. Focuses the terminal after pasting, unless focus has moved since the call (into a dialog, say).
 */
export async function pasteIntoSession(id: string, text: string, timeoutMs = 2000): Promise<boolean> {
  const focused = document.activeElement
  const deadline = Date.now() + timeoutMs
  while (!handles.get(id)?.ready()) {
    if (Date.now() >= deadline) return false
    await new Promise((resolve) => setTimeout(resolve, 50))
  }
  const handle = handles.get(id)!
  handle.paste(text)
  if (document.activeElement === focused || document.activeElement === document.body) handle.focus()
  return true
}
