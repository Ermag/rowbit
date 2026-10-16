<script>
  import { onMount } from 'svelte';
  import * as api from './api.js';
  import { ago } from './format.js';

  let list = $state(null);
  let error = $state('');

  async function load() {
    try {
      list = (await api.deliveries()).deliveries;
      error = '';
    } catch (e) {
      error = e.message;
    }
  }

  onMount(() => {
    load();
    const timer = setInterval(load, 10000);
    return () => clearInterval(timer);
  });
</script>

<h1>Notifications</h1>
<p class="muted">
  Held changes announced to the targets in <code>rowbit-server.yml</code>. A notification that
  can't be delivered never affects the change; it stays held here.
</p>
{#if error}<p class="error">{error}</p>{/if}
{#if list && list.length === 0}<p class="muted">Nothing sent yet.</p>{/if}
{#if list && list.length}
  <div class="scroll">
    <table>
      <thead><tr><th>Change</th><th>Target</th><th>State</th><th>Tries</th><th>Last error</th><th>Queued</th></tr></thead>
      <tbody>
        {#each list as d (d.id)}
          <tr>
            <td><a href="#/changes/{d.change_id}">#{d.change_id}</a></td>
            <td>{d.target}</td>
            <td><span class="state {d.state === 'delivered' ? 'applied' : d.state === 'failed' ? 'failed' : 'held'}">{d.state}</span></td>
            <td>{d.attempts}</td>
            <td>{d.last_error ?? ''}</td>
            <td class="nowrap">{ago(d.created_at_ms)}</td>
          </tr>
        {/each}
      </tbody>
    </table>
  </div>
{/if}
