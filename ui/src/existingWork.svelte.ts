import { api, type BranchWork, type WorkSubject } from './api'

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
      this.work = null
      this.checkedAt = null
    }
    const request = ++this.#request
    this.loading = true
    try {
      const result = await api.existingWork(subject, fetch)
      if (request === this.#request) {
        this.work = result
        this.checkedAt = Date.now()
      }
    } catch (e) {
      if (request === this.#request) this.#onError(String(e))
    } finally {
      if (request === this.#request) this.loading = false
    }
  }
}
