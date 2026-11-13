import { api } from './api'
import { ask, type AskOptions } from './confirm.svelte'

export const overlay = $state({ open: 0 })

export async function confirmOverBrowser(options: AskOptions): Promise<boolean> {
  overlay.open++
  try {
    await api.browserShow(null)
    return await ask(options)
  } finally {
    overlay.open--
  }
}
