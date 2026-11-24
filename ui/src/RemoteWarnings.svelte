<script lang="ts">
  interface Props {
    warnings: string[]
  }

  let { warnings }: Props = $props()
</script>

{#if warnings.length}
  <ul class="warnings">
    {#each warnings as warning (warning)}<li>{warning}</li>{/each}
  </ul>
{/if}

<style>
  .warnings {
    margin: 0;
    padding-left: 18px;
    color: var(--warn);
  }
</style>
