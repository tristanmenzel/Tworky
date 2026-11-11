<script lang="ts">
  import { api, type AccountRequest, type Worker } from './api'

  interface Props {
    worker: Worker
    onCreate: (request: AccountRequest) => void
    onError: (message: string) => void
  }

  let { worker, onCreate, onError }: Props = $props()

  async function dismiss() {
    try {
      await api.dismissAccountRequests(worker.id)
    } catch (e) {
      onError(String(e))
    }
  }
</script>

<section class="requests">
  <div class="head">
    <strong>Needs {worker.account_requests.length === 1 ? 'an account' : 'accounts'}</strong>
    <span class="muted">{worker.id}</span>
    <span class="buttons">
      <button onclick={dismiss} title="Clear the requests; sending Claude a prompt clears them too">Dismiss</button>
    </span>
  </div>
  <ul>
    {#each worker.account_requests as r (r.name)}
      <li>
        <div class="what">
          <div>
            <strong>{r.name}</strong>
            <span class="muted">{r.username} · starts on {r.service}</span>
          </div>
          {#if r.reason}<div class="reason">{r.reason}</div>{/if}
        </div>
        <button class="primary" onclick={() => onCreate(r)}>Create</button>
      </li>
    {/each}
  </ul>
</section>

<style>
  .requests {
    padding: 12px 14px;
    margin-bottom: 12px;
    background: color-mix(in srgb, var(--account) 8%, var(--panel));
    border: 1px solid color-mix(in srgb, var(--account) 45%, var(--border));
    border-radius: 8px;
  }
  .head {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .buttons {
    margin-left: auto;
    display: flex;
    gap: 6px;
  }
  ul {
    list-style: none;
    margin: 8px 0 0;
    padding: 0;
  }
  li {
    display: flex;
    align-items: center;
    gap: 12px;
  }
  li + li {
    margin-top: 8px;
  }
  .what {
    flex: 1;
    min-width: 0;
  }
  .reason {
    margin-top: 2px;
  }
</style>
