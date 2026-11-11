<script lang="ts">
  import type { BoardStatus, Issue } from './api'
  import AutoSelect from './AutoSelect.svelte'
  import { boardTone } from './issue'

  interface Props {
    issue: Issue
    boards: BoardStatus[]
    onChange: (issueId: string, projectId: string, optionId: string) => Promise<void>
  }

  let { issue, boards, onChange }: Props = $props()
</script>

{#each boards as b (b.project_id)}
  {@const current = b.options.find((o) => o.id === b.option_id)}
  <AutoSelect
    value={b.option_id}
    options={b.options.map((o) => ({ value: o.id, label: boards.length > 1 ? `${b.title}: ${o.name}` : o.name }))}
    placeholder={boards.length > 1 ? `${b.title}: no status` : 'No status'}
    label={`Status on ${b.title}`}
    tone={boardTone(current?.name)}
    onChange={(optionId) => onChange(issue.id, b.project_id, optionId)}
  />
{/each}
