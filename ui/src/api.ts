import { Channel, invoke } from '@tauri-apps/api/core'

export interface Issue {
  id: string
  number: number
  title: string
  url: string
  state: string
  state_reason: string | null
  body: string
  issue_type: string | null
  repo: string
  assignees: string[]
  parent_id: string | null
  sub_issues_total: number
  sub_issues_completed: number
  boards: BoardStatus[]
}

export interface BoardStatus {
  item_id: string
  project_id: string
  number: number
  title: string
  field_id: string
  option_id: string | null
  options: { id: string; name: string }[]
}

export interface Card {
  id: string
  /** The work item that put the issue on the list. */
  item: string
  ancestors: string[]
}

export interface IssuesPayload {
  viewer: string
  issues: Issue[]
  cards: Card[]
  projects: Record<string, string>
  subtasks: string[]
  status_boards: Record<string, number>
  config_error: string | null
}

export type IssueState = 'open' | 'completed' | 'not_planned'

export type WorkerStatus = 'creating' | 'ready' | 'failed' | 'cleaning'

/**
 * What a worker's live Claude is doing, reported by its session hooks. `uat` is idle with a handover, and `account`
 * idle with account requests.
 */
export type Activity = 'working' | 'idle' | 'waiting' | 'uat' | 'account'

export interface Handover {
  summary: string
  steps: string[]
  tabs: number[]
}

export interface AccountRequest {
  name: string
  username: string
  service: string
  reason: string
}

export interface Worker {
  id: string
  project: string
  issue: { id: string; number: number; repo: string; title: string; url: string } | null
  item: { name: string; title: string; description: string } | null
  pr: PrRef | null
  branch: string
  slot: number
  ports: Record<string, number>
  dir: string
  compose_project: string
  status: WorkerStatus
  claude_session: string
  claude_started: boolean
  error: string | null
  created_at: number
  worktrees: { repo: string; branch: string; origin: 'new' | 'tracking' | 'existing' }[]
  session_alive: boolean
  busy: boolean
  activity: Activity | null
  handover: Handover | null
  account_requests: AccountRequest[]
}

/** A dev server, watcher or test run still running in a worker. */
export interface Job {
  pid: number
  command: string
  from_claude: boolean
  age_secs: number
  memory_bytes: number
  process_count: number
  roots: number[]
  output: string | null
}

export interface OutputChunk {
  text: string
  offset: number
}

export interface LocalStatus {
  worktrees: LocalWorktree[]
  services: ServiceStatus[]
  stack: ContainerStatus[] | null
  stack_error: string | null
  listeners: PortListener[]
}

/** Something listening on one of the worker's ports that isn't a service or the stack. */
export interface PortListener {
  name: string
  port: number
  pid: number | null
  command: string | null
}

export interface LocalWorktree {
  repo: string
  branch: string
  dirty_files: number
  unpushed: number
  error: string | null
}

export interface ServiceStatus {
  name: string
  port: number | null
  url: string | null
  needs: string[]
  running: boolean
  /** `starting` while its process runs but nothing answers on its port yet. */
  state: ServiceState
  /** Set only for a service Tworky started whose process is still running its command. */
  pid: number | null
  log: string | null
}

export type ServiceState = 'running' | 'starting' | 'stopped'

export interface ContainerStatus {
  service: string
  state: string
  health: string | null
  ports: { published: number; target: number }[]
}

export interface PullRequest {
  number: number
  title: string
  url: string
  state: string
  isDraft: boolean
}

export interface PrSummary {
  repos: {
    repo: string
    branch: string
    name_with_owner: string | null
    pull_requests: PullRequest[]
    status: { dirty_files: number; unpushed_commits: number } | null
    error: string | null
  }[]
  all_merged: boolean
}

export interface FailingCheck {
  name: string
  url: string | null
}

export interface PrDetail {
  repo: string
  number: number
  title: string
  url: string
  state: 'OPEN' | 'MERGED' | 'CLOSED'
  is_draft: boolean
  head: string
  base: string
  mergeable: string
  merge_state: string
  updated_at: string
  checks: { state: string | null; failing: FailingCheck[]; pending: number }
  unresolved_threads: number
  approved_by: string[]
  changes_requested_by: string[]
  review_requests: number
}

export type Verdict = 'needs_attention' | 'draft' | 'ready' | 'in_review'

export type WorkSubject = { kind: 'issue'; id: string } | { kind: 'item'; name: string }

export interface BranchWork {
  repo: string
  name_with_owner: string | null
  branch: string
  local: boolean
  remote: boolean
  ahead: number | null
  behind: number | null
  unpushed: number | null
  last_commit: string | null
  worker: string | null
  matched: number[]
  pull_requests: PrDetail[]
  signals: { level: 'ok' | 'info' | 'warn' | 'bad'; text: string }[]
  verdict: Verdict | null
}

export interface IssueRef {
  id: string
  number: number
  repo: string
  title: string
  url: string
}

export interface PrRef {
  number: number
  repo: string
  title: string
  url: string
  head: string
  base: string
}

export type ItemSource = { kind: 'custom' } | ({ kind: 'issue' } & IssueRef) | ({ kind: 'pull_request' } & PrRef)

export interface Item {
  name: string
  title: string
  description: string
  project: string
  created_at: number
  source: ItemSource
  scratch: boolean
}

export interface IssueHit {
  id: string
  number: number
  title: string
  repo: string
  url: string
  state: string
  assignees: string[]
  issue_type: string | null
  parent_id: string | null
  /** Found by the search, rather than only as an ancestor of a match. */
  matched: boolean
  /** The work item it's already listed as. */
  item: string | null
}

export interface PrHit {
  number: number
  title: string
  repo: string
  url: string
  author: string
  head: string
  base: string
  is_draft: boolean
  updated_at: string
  item: string | null
}

export type ProjectChoice = { kind: 'infer' } | { kind: 'existing'; name: string } | { kind: 'new'; name: string | null }

export type AddOutcome =
  | { outcome: 'added'; item: Item; new_project: string | null }
  | { outcome: 'needs_project'; repo: string }

export interface ProjectSummary {
  name: string
  file: string
  memory_file: string
  match: { owner: string | null; repos: string[] }
  subtask_types: string[]
  no_rollup_into: string[]
  context_types: string[]
  branch: string
  session_cwd: string
  repos: { name: string; source: string; base: string }[]
  ports: Record<string, number>
  stack: { compose_dir: string; compose_files: string[]; env_file: string | null; profiles: string[] } | null
  hooks: { setup: { cwd: string; run: Command }[]; teardown: { cwd: string; run: Command }[] }
  template_dir: string | null
  accounts: Account[]
  services: Service[]
  notes: string
  status_board: number | null
  onboarding: boolean
  /** The id its setup session's terminal runs under. */
  setup_session: string
  setup_alive: boolean
  setup_started: boolean
}

/** A command for every OS, or one per OS. */
export type Command = string | Partial<Record<'windows' | 'unix' | 'macos' | 'linux', string>>

export interface Service {
  name: string
  cwd: string
  run: Command
  port: string | null
  https: boolean
  needs: string[]
  ready_timeout_secs: number
}

export interface Account {
  name: string
  username: string
  start_url: string
  start_service: string | null
  mobile: boolean
  login_origins: string[]
  color: string | null
}

export interface AccountView extends Account {
  has_password: boolean
}

export interface AccountsPayload {
  accounts: AccountView[]
}

export interface BrowserTabView {
  id: number
  worker: string
  project: string
  /** Null for an anonymous tab. */
  account: string | null
  /** The account's username (its email, for a profile made with Create). */
  username: string | null
  color: string | null
  mobile: boolean
  url: string
  loading: boolean
  can_sign_in: boolean
  login_fields: ('username' | 'password')[]
  /** Only looked up while the page's login form has a password field. */
  has_password: boolean
  can_fill_email: boolean
  /** The page's service starting, or why it didn't. */
  status: string | null
  /** The page isn't loaded because the service serving it isn't running. */
  paused: { service: string; port: number; url: string } | null
}

export interface Bounds {
  x: number
  y: number
  width: number
  height: number
}

export interface RepoSearch {
  candidates: { name_with_owner: string; description: string; local: string | null }[]
  search_error: string | null
}

export interface ProjectMemory {
  project: string
  path: string
  text: string
}

export type UpdateStatus =
  | { kind: 'latest' }
  | { kind: 'behind'; commits: number; tag: string }
  | { kind: 'unknown'; reason: string }

export interface VersionInfo {
  commit: string
  short: string
  dev: boolean
  status: UpdateStatus | null
  error: string | null
}

export interface UpdateProgress {
  progress: { phase: 'downloading' | 'ready' | 'installing' | 'done' | 'failed'; message: string } | null
  log_tail: string
}

// The host ignores a show older than one it applied from the same load.
const showLoad = Math.floor(Math.random() * Number.MAX_SAFE_INTEGER)
let showSeq = 0

export const api = {
  updateStatus: () => invoke<VersionInfo>('update_status'),
  updateProgress: () => invoke<UpdateProgress>('update_progress'),
  dismissUpdateFailure: () => invoke<void>('dismiss_update_failure'),
  startUpdate: () => invoke<void>('start_update'),
  restartForUpdate: () => invoke<void>('restart_for_update'),
  projects: () => invoke<ProjectSummary[]>('list_projects'),
  browserTabs: () => invoke<BrowserTabView[]>('browser_tabs'),
  browserOpen: (worker: string, account: string | null, url: string | null = null) =>
    invoke<number>('browser_open', { worker, account, url }),
  browserNavigate: (tab: number, url: string) => invoke<unknown>('browser_navigate', { tab, url }),
  browserHistory: (tab: number, action: 'back' | 'forward' | 'reload') =>
    invoke<void>('browser_history', { tab, action }),
  browserBounds: (tab: number, bounds: Bounds) => invoke<void>('browser_bounds', { tab, bounds }),
  browserShow: (tab: number | null) => invoke<void>('browser_show', { tab, load: showLoad, seq: ++showSeq }),
  browserClose: (tab: number) => invoke<void>('browser_close', { tab }),
  browserSignIn: (tab: number) => invoke<unknown>('browser_sign_in', { tab }),
  /** Returns the page's origin. With `origin`, refuses if the page has left it. */
  browserFillEmail: (tab: number, persist: boolean, origin: string | null = null) =>
    invoke<string>('browser_fill_email', { tab, persist, origin }),
  browserEndSession: (tab: number) => invoke<unknown>('browser_end_session', { tab }),
  browserStartService: (tab: number) => invoke<void>('browser_start_service', { tab }),
  browserRetry: (tab: number) => invoke<void>('browser_retry', { tab }),
  accounts: (project: string) => invoke<AccountsPayload>('list_accounts', { project }),
  saveAccount: (project: string, previous: string | null, account: Account) =>
    invoke<void>('save_account', { project, previous, account }),
  removeAccount: (project: string, name: string) => invoke<void>('remove_account', { project, name }),
  setAccountPassword: (project: string, name: string, password: string) =>
    invoke<void>('set_account_password', { project, name, password }),
  searchRepos: (query: string) => invoke<RepoSearch>('search_repos', { query }),
  items: () => invoke<Item[]>('list_items'),
  createItem: (
    title: string,
    description: string,
    project: string | null,
    repos: string[],
    projectName: string | null = null,
  ) => invoke<AddOutcome>('create_item', { title, description, project, repos, projectName }),
  createProject: (repos: string[], name: string | null) => invoke<string>('create_project', { repos, name }),
  searchIssues: (query: string) => invoke<IssueHit[]>('search_issues', { query }),
  reviewRequests: () => invoke<PrHit[]>('review_requests'),
  addIssueItem: (url: string, choice: ProjectChoice) => invoke<AddOutcome>('add_issue_item', { url, choice }),
  addPullRequestItem: (url: string, choice: ProjectChoice) =>
    invoke<AddOutcome>('add_pull_request_item', { url, choice }),
  beginItem: (name: string) => invoke<Worker>('begin_item_session', { name }),
  linkItem: (name: string, issueUrl: string) => invoke<void>('link_item', { name, issueUrl }),
  removeItem: (name: string) => invoke<void>('remove_item', { name }),
  memories: () => invoke<ProjectMemory[]>('list_memories'),
  saveMemory: (project: string, text: string) => invoke<void>('save_memory', { project, text }),
  existingWork: (subject: WorkSubject, refresh: boolean) =>
    invoke<BranchWork[]>('existing_work', { subject, refresh }),
  deleteBranch: (subject: WorkSubject, repo: string, branch: string) =>
    invoke<string[]>('delete_branch', { subject, repo, branch }),
  issues: (refresh: boolean) => invoke<IssuesPayload>('get_issues', { refresh }),
  workers: () => invoke<Worker[]>('list_workers'),
  workerLog: (id: string) => invoke<string[]>('worker_log', { id }),
  jobs: () => invoke<Record<string, Job[]>>('list_jobs'),
  stopJob: (id: string, pid: number) => invoke<void>('stop_job', { id, pid }),
  workerLocal: (id: string) => invoke<LocalStatus>('worker_local', { id }),
  stopService: (id: string, name: string) => invoke<void>('stop_service', { id, name }),
  stopStack: (id: string) => invoke<void>('stop_stack', { id }),
  jobOutput: (path: string, offset: number | null) => invoke<OutputChunk>('job_output', { path, offset }),
  begin: (issueId: string) => invoke<Worker>('begin_session', { issueId }),
  assign: (issueId: string) => invoke<IssuesPayload>('assign_to_me', { issueId }),
  setIssueState: (issueId: string, state: IssueState) =>
    invoke<IssuesPayload>('set_issue_state', { issueId, state }),
  setBoardStatus: (issueId: string, projectId: string, optionId: string) =>
    invoke<IssuesPayload>('set_board_status', { issueId, projectId, optionId }),
  cleanup: (id: string) => invoke<void>('cleanup_worker', { id }),
  prs: (id: string) => invoke<PrSummary>('pr_summary', { id }),
  openSession: (id: string, cols: number, rows: number, onOutput: Channel<string>) =>
    invoke<void>('open_session', { id, cols, rows, onOutput }),
  write: (id: string, data: string) => invoke<void>('write_session', { id, data }),
  resize: (id: string, cols: number, rows: number) => invoke<void>('resize_session', { id, cols, rows }),
  stop: (id: string) => invoke<void>('stop_session', { id }),
  newConversation: (id: string) => invoke<void>('new_conversation', { id }),
  openDir: (id: string) => invoke<void>('open_worker_dir', { id }),
  openInVscode: (id: string, repo: string) => invoke<void>('open_in_vscode', { id, repo }),
  dismissHandover: (id: string) => invoke<void>('dismiss_handover', { id }),
  dismissAccountRequest: (id: string, name: string) => invoke<void>('dismiss_account_request', { id, name }),
  dismissAccountRequests: (id: string) => invoke<void>('dismiss_account_requests', { id }),
  openProjectConfig: (name: string) => invoke<void>('open_project_config', { name }),
}
