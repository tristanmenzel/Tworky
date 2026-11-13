import { overlay } from './overlay.svelte'

export interface AskOptions {
  title: string
  message?: string
  warning?: { title: string; message?: string | string[] }
  confirmLabel: string
  danger?: boolean
}

interface Request extends AskOptions {
  resolve: (ok: boolean) => void
}

export const confirmation = $state<{ request: Request | null }>({ request: null })

export function answer(ok: boolean) {
  const request = confirmation.request
  if (!request) return
  confirmation.request = null
  overlay.open--
  request.resolve(ok)
}

export function ask(options: AskOptions): Promise<boolean> {
  answer(false)
  overlay.open++
  return new Promise((resolve) => {
    confirmation.request = { ...options, resolve }
  })
}
