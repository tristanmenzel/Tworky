<script lang="ts">
  export interface ServiceLink {
    name: string
    url: string
  }

  interface Props {
    services: ServiceLink[]
    picked: string | null
    disabled?: boolean
  }

  let { services, picked = $bindable(), disabled = false }: Props = $props()
</script>

<div class="services">
  {#each services as s (s.name)}
    <label class="choice">
      <input type="radio" name="service" value={s.name} bind:group={picked} {disabled} />
      <strong>{s.name}</strong>
      <span class="muted">- {s.url}</span>
    </label>
  {/each}
</div>

<style>
  .services {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .choice {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 4px 6px;
    border-radius: 6px;
    cursor: pointer;
  }
  .choice:hover {
    background: var(--panel-2);
  }
  .muted {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    min-width: 0;
  }
</style>
