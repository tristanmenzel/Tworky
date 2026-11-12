import { api, type BranchWork, type WorkSubject } from './api'

const lastKnown = new Map<string, { work: BranchWork[]; checkedAt: number; requestedAt: number }>()

export class ExistingWorkLoader {
  work = $state<BranchWork[] | null>(null)
  loading = $state(false)
  checkedAt = $state<number | null>(null)
  #key = ''
  #request = 0
  #onError: (message: string) => void

  constructor(onError: (message: string) => void) {
    this.#onError = onError
  }

  async load(subject: WorkSubject, fetch: boolean) {
    const key = subject.kind === 'issue' ? `issue:${subject.id}` : `item:${subject.name}`
    if (key !== this.#key) {
      this.#key = key
      const cached = lastKnown.get(key)
      this.work = cached?.work ?? null
      this.checkedAt = cached?.checkedAt ?? null
    }
    const request = ++this.#request
    const requestedAt = Date.now()
    this.loading = true
    try {
      const result = await api.existingWork(subject, fetch)
      if (request === this.#request) {
        this.work = result
        this.checkedAt = Date.now()
      }
      if ((lastKnown.get(key)?.requestedAt ?? 0) <= requestedAt) {
        lastKnown.set(key, { work: result, checkedAt: Date.now(), requestedAt })
      }
    } catch (e) {
      if (request === this.#request) this.#onError(String(e))
    } finally {
      if (request === this.#request) this.loading = false
    }
  }
}
