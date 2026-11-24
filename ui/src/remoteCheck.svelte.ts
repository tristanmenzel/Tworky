import type { ConfigConnected, ConnectOutcome, RemoteCheck, RemoteSeen } from './api'

/**
 * Connects the config to a remote in two steps when Tworky has warnings about it: the first call checks it and, if
 * there are warnings, keeps them for the user to read and returns null; the next call connects, sending what the user
 * saw. When the server's own check finds more than that, its new check is kept to show and the call returns null
 * again. Call `reset` when the remote changes.
 */
export class RemoteConfirmation {
  check = $state<RemoteCheck | null>(null)

  reset() {
    this.check = null
  }

  async connect(
    check: () => Promise<RemoteCheck>,
    connect: (seen: RemoteSeen) => Promise<ConnectOutcome>,
  ): Promise<ConfigConnected | null> {
    let seen = this.check
    if (!seen) {
      const checked = await check()
      if (checked.warnings.length) {
        this.check = checked
        return null
      }
      seen = checked
    }
    const outcome = await connect({ visibility: seen.visibility, foreign_owner: seen.foreign_owner })
    if (outcome.kind === 'changed') {
      this.check = outcome.check
      return null
    }
    return outcome.connected
  }
}
