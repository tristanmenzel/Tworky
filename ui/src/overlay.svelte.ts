import { api } from './api'

export const overlay = $state({ open: 0 })

export async function confirmOverBrowser(message: string): Promise<boolean> {
  overlay.open++
  try {
    await api.browserShow(null)
    return confirm(message)
  } finally {
    overlay.open--
  }
}
