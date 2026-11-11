<script lang="ts">
  import type { Activity, Worker } from './api'
  import { activityLabel, activityOf } from './issue'

  let { worker, activity: override, title }: { worker?: Worker; activity?: Activity | null; title?: string } = $props()

  const activity = $derived(override !== undefined ? override : worker ? activityOf(worker) : null)
</script>

<span
  class="dot {worker?.status ?? ''} {activity ?? ''}"
  class:live={!!activity || !!worker?.session_alive}
  title={[title, activity && activityLabel(activity)].filter(Boolean).join(' · ') || undefined}
></span>
