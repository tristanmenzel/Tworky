<script lang="ts">
  import { api, type Command, type Item, type ProjectSummary, type Worker } from './api'
  import { subjectLabel } from './issue'
  import Markdown from './Markdown.svelte'
  import WorkerDot from './WorkerDot.svelte'

  interface Props {
    project: ProjectSummary
    workers: Worker[]
    items: Item[]
    onShowWorker: (worker: Worker) => void
    onOpenSetup: () => void
    onError: (message: string) => void
  }

  let { project, workers, items, onShowWorker, onOpenSetup, onError }: Props = $props()

  const ports = $derived(Object.entries(project.ports))
  const hooks = $derived([
    ...project.hooks.setup.map((h) => ({ ...h, kind: 'setup' })),
    ...project.hooks.teardown.map((h) => ({ ...h, kind: 'teardown' })),
  ])

  function commandText(run: Command): string {
    if (typeof run === 'string') return run
    return Object.entries(run)
      .map(([os, command]) => `${os}: ${command}`)
      .join('\n')
  }

  async function openConfig() {
    try {
      await api.openProjectConfig(project.name)
    } catch (e) {
      onError(String(e))
    }
  }
</script>

<article>
  <header>
    <div>
      <h1>{project.name}</h1>
      <div class="muted small"><code>{project.file}</code></div>
    </div>
    <div class="actions">
      <button class:primary={project.onboarding} onclick={onOpenSetup}>
        {project.onboarding ? 'Set up with Claude' : project.setup_started ? 'Reopen setup session' : 'Adjust with Claude'}
      </button>
      <button onclick={openConfig}>Open config</button>
    </div>
  </header>

  {#if project.onboarding}
    <section class="box onboarding">
      <strong>Not set up yet.</strong>
      <span class="muted">
        Tworky made this project with just its repos. A setup session asks about the rest of the project, reads its
        docs and configures ports, the stack, hooks and services, then tests them on a scratch item.
      </span>
    </section>
  {/if}

  <section class="box">
    <h2>Matching</h2>
    <dl>
      {#if project.match.owner}<dt>Owner</dt><dd><code>{project.match.owner}</code></dd>{/if}
      {#if project.match.repos.length}
        <dt>Repos</dt><dd>{#each project.match.repos as r (r)}<code>{r}</code> {/each}</dd>
      {/if}
      {#if project.status_board != null}<dt>Status board</dt><dd>#{project.status_board}</dd>{/if}
      {#if project.subtask_types.length}<dt>Subtask types</dt><dd>{project.subtask_types.join(', ')}</dd>{/if}
      {#if project.context_types.length}<dt>Context types</dt><dd>{project.context_types.join(', ')}</dd>{/if}
    </dl>
  </section>

  <section class="box">
    <h2>Repositories</h2>
    <table>
      <thead><tr><th>Name</th><th>Source</th><th>Base</th></tr></thead>
      <tbody>
        {#each project.repos as r (r.name)}
          <tr><td>{r.name}</td><td><code>{r.source}</code></td><td><code>{r.base}</code></td></tr>
        {/each}
      </tbody>
    </table>
  </section>

  <section class="box">
    <h2>Workers</h2>
    <dl>
      <dt>Branch</dt><dd><code>{project.branch}</code></dd>
      <dt>Session folder</dt><dd><code>{project.session_cwd}</code></dd>
      {#if project.stack}
        <dt>Stack</dt>
        <dd>
          <code>{project.stack.compose_dir}</code>
          {#if project.stack.compose_files.length}<span class="muted">{project.stack.compose_files.join(', ')}</span>{/if}
          {#if project.stack.profiles.length}<span class="muted">profiles {project.stack.profiles.join(', ')}</span>{/if}
        </dd>
      {/if}
    </dl>
    {#if ports.length}
      <h3>Base ports</h3>
      <div class="ports">
        {#each ports as [name, port] (name)}<span><span class="muted">{name}</span> <code>{port}</code></span>{/each}
      </div>
    {/if}
    {#if project.services.length}
      <h3>Services</h3>
      <table>
        <tbody>
          {#each project.services as s (s.name)}
            <tr>
              <td class="nowrap">
                {s.name}
                {#if s.port}<span class="muted">:{s.port}</span>{/if}
                {#if s.needs.length}<div class="muted small">needs {s.needs.join(', ')}</div>{/if}
              </td>
              <td class="nowrap"><code>{s.cwd}</code></td>
              <td><code class="pre">{commandText(s.run)}</code></td>
            </tr>
          {/each}
        </tbody>
      </table>
    {/if}
    {#if hooks.length}
      <h3>Hooks</h3>
      <table>
        <tbody>
          {#each hooks as h, i (i)}
            <tr><td class="muted">{h.kind}</td><td class="nowrap"><code>{h.cwd}</code></td><td><code class="pre">{commandText(h.run)}</code></td></tr>
          {/each}
        </tbody>
      </table>
    {/if}
  </section>

  <section class="box">
    <h2>Active work</h2>
    {#each workers as w (w.id)}
      <button class="row link" onclick={() => onShowWorker(w)}>
        <WorkerDot worker={w} />
        <code>{w.id}</code>
        <span class="muted">{subjectLabel(w)}</span>
      </button>
    {:else}
      <p class="muted">No workers.</p>
    {/each}
    {#if items.length}
      <p class="muted small">{items.length} item{items.length === 1 ? '' : 's'}: {items.map((i) => i.name).join(', ')}</p>
    {/if}
  </section>

  {#if project.notes.trim()}
    <section class="box">
      <h2>Notes for Claude</h2>
      <Markdown source={project.notes} />
    </section>
  {/if}
</article>

<style>
  article {
    padding: 16px 24px 32px;
    max-width: 980px;
    display: flex;
    flex-direction: column;
    gap: 14px;
  }
  header {
    display: flex;
    justify-content: space-between;
    gap: 16px;
    align-items: flex-start;
  }
  .actions {
    display: flex;
    gap: 6px;
    flex: none;
  }
  .onboarding {
    display: flex;
    flex-direction: column;
    gap: 4px;
    border-color: var(--warn);
  }
  h1 {
    font-size: 20px;
    margin: 0 0 2px;
    font-weight: 600;
  }
  h2,
  h3 {
    font-size: 12px;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--muted);
    margin: 0 0 6px;
    font-weight: 600;
  }
  h3 {
    margin-top: 10px;
    font-size: 11px;
  }
  .small {
    font-size: 12px;
  }
  .box {
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--panel);
    padding: 10px 14px;
  }
  .box p {
    margin: 4px 0 0;
  }
  dl {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 4px 16px;
    margin: 0;
  }
  dt {
    color: var(--muted);
  }
  dd {
    margin: 0;
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    min-width: 0;
  }
  .pre {
    white-space: pre-wrap;
  }
  table {
    border-collapse: collapse;
    width: 100%;
  }
  th {
    text-align: left;
    font-weight: 600;
    color: var(--muted);
    font-size: 12px;
  }
  th,
  td {
    padding: 3px 12px 3px 0;
    vertical-align: top;
  }
  td code {
    word-break: break-all;
  }
  td.nowrap code {
    white-space: nowrap;
    word-break: normal;
  }
  .ports {
    display: flex;
    flex-wrap: wrap;
    gap: 4px 16px;
  }
  .row {
    display: flex;
    gap: 8px;
    align-items: center;
    width: 100%;
    text-align: left;
    padding: 3px 4px;
    white-space: nowrap;
    overflow: hidden;
  }
  .row .muted {
    overflow: hidden;
    text-overflow: ellipsis;
  }
</style>
