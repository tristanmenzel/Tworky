import { api, type RemoteCheck } from './api'

/**
 * Connects the config to a remote in two steps when Tworky has warnings about it: the first call checks it and, if
 * there are warnings, keeps them for the user to read and returns false; the next call connects, confirming a remote
 * that needs it. Call `reset` when the remote changes.
 */
export class RemoteConfirmation {
  check = $state<RemoteCheck | null>(null)

  reset() {
    this.check = null
  }

  async connect(remote: string, connect: (confirmed: boolean) => Promise<void>): Promise<boolean> {
    if (!this.check) {
      const checked = await api.checkConfigRemote(remote)
      if (checked.warnings.length) {
        this.check = checked
        return false
      }
    }
    await connect(this.check?.needs_confirmation ?? false)
    return true
  }
}
