<script lang="ts">
  import type { Issue, IssueState } from './api'
  import AutoSelect from './AutoSelect.svelte'
  import { issueState } from './issue'

  interface Props {
    issue: Issue
    onChange: (issueId: string, state: IssueState) => Promise<void>
  }

  let { issue, onChange }: Props = $props()

  const OPTIONS: { value: IssueState; label: string }[] = [
    { value: 'open', label: 'Open' },
    { value: 'completed', label: 'Closed: done' },
    { value: 'not_planned', label: 'Closed: not planned' },
  ]

  const state = $derived(issueState(issue))
</script>

<AutoSelect
  value={state}
  options={OPTIONS}
  placeholder={issue.state_reason?.toLowerCase().replace('_', ' ') ?? 'closed'}
  label="Issue state"
  tone={state === 'open' ? 'doing' : state === 'completed' ? 'done' : 'todo'}
  onChange={(v) => onChange(issue.id, v as IssueState)}
/>
