const pasters = new Map<string, (text: string) => void>()

export function registerTerminal(id: string, paste: (text: string) => void): () => void {
  pasters.set(id, paste)
  return () => {
    if (pasters.get(id) === paste) pasters.delete(id)
  }
}

/** Waits up to `timeoutMs` for a terminal that is still attaching; false if none attaches. Doesn't submit. */
export async function pasteIntoSession(id: string, text: string, timeoutMs = 2000): Promise<boolean> {
  const deadline = Date.now() + timeoutMs
  while (!pasters.has(id)) {
    if (Date.now() >= deadline) return false
    await new Promise((resolve) => setTimeout(resolve, 50))
  }
  pasters.get(id)!(text)
  return true
}
