interface Paster {
  paste: (text: string) => void
  /** True once the program in the terminal reads input, which Claude Code signals by enabling bracketed paste. */
  ready: () => boolean
}

const pasters = new Map<string, Paster>()

export function registerTerminal(id: string, paster: Paster): () => void {
  pasters.set(id, paster)
  return () => {
    if (pasters.get(id) === paster) pasters.delete(id)
  }
}

/** Waits up to `timeoutMs` for the terminal to attach and Claude to take input; false if it doesn't. Doesn't submit. */
export async function pasteIntoSession(id: string, text: string, timeoutMs = 2000): Promise<boolean> {
  const deadline = Date.now() + timeoutMs
  while (!pasters.get(id)?.ready()) {
    if (Date.now() >= deadline) return false
    await new Promise((resolve) => setTimeout(resolve, 50))
  }
  pasters.get(id)!.paste(text)
  return true
}
