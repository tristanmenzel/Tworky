import type { ConfigSyncState } from './api'

/** A stopped sync's one-line description, or null while nothing needs the user. */
export function syncProblem(state: ConfigSyncState): string | null {
  switch (state.kind) {
    case 'conflict':
      return `Config sync stopped on a conflict in ${fileCount(state.files)}`
    case 'held_back':
      return state.cause === 'unrecognised'
        ? `Config sync held back project config this Tworky doesn't understand, probably from a newer Tworky: update Tworky`
        : `Config sync held back project config from the remote that doesn't parse`
    case 'held_in_sync_worktree':
      return `Config syncs are on hold: ${fileCount(state.files)} in the sync worktree aren't committed`
    case 'blocked_by_local':
      return `Config sync is blocked: incoming changes would overwrite ${fileCount(state.files)} not committed here`
    case 'error':
      return `Config sync failed: ${state.message}`
    case 'unconfirmed_remote':
      return `Config sync stopped: the remote ${state.url} wasn't connected through Tworky`
    default:
      return null
  }
}

function fileCount(files: string[]): string {
  return files.length === 1 ? '1 file' : `${files.length} files`
}

export function formatSyncTime(seconds: number | null): string {
  return seconds === null ? 'never' : new Date(seconds * 1000).toLocaleString()
}
