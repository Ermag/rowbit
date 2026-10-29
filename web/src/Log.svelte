<script>
  import { onMount } from 'svelte';
  import * as api from './api.js';
  import { ago, verbs } from './format.js';
  import State from './State.svelte';

  const states = ['', 'proposed', 'held', 'approved', 'applied', 'rejected', 'failed', 'undone'];
  let filters = $state({ state: '', agent: '', table: '' });
  let list = $state([]);
  let verified = $state(null);
  let error = $state('');

  async function load() {
    try {
      list = (await api.changes(filters)).changes;
      error = '';
    } catch (e) {
      error = e.message;
    }
  }

  onMount(async () => {
    load();
    verified = await api.verify().catch((e) => ({ ok: false, problem: e.message }));
  });
</script>

<h1>Log</h1>
{#if verified}
  {#if verified.ok}
    <p class="ok">The audit log's hash chain checks out.</p>
  {:else}
    <p class="error" role="alert">The audit log doesn't check out: {verified.problem}</p>
  {/if}
{/if}

<form
  class="filters"
  onsubmit={(e) => {
    e.preventDefault();
    load();
  }}
>
  <label
    >State
    <select bind:value={filters.state}>
      {#each states as s (s)}<option value={s}>{s || 'any'}</option>{/each}
    </select></label
  >
  <label>Agent <input bind:value={filters.agent} /></label>
  <label>Table <input bind:value={filters.table} placeholder="db.table" /></label>
  <button>Filter</button>
</form>
{#if error}<p class="error">{error}</p>{/if}

<div class="scroll">
  <table>
    <thead
      ><tr><th>#</th><th>State</th><th>Agent</th><th>What</th><th>Decided by</th><th>Reason</th><th>When</th></tr
      ></thead
    >
    <tbody>
      {#each list as c (c.id)}
        <tr>
          <td><a href="#/changes/{c.id}">{c.id}</a></td>
          <td><State state={c.state} /></td>
          <td>{c.agent}</td>
          <td
            >{verbs[c.kind] ?? ''}
            {#if c.rows != null}{c.rows}{/if}
            {#if c.table}<code>{c.table}</code>{/if}</td
          >
          <td>{c.decided_by ?? ''}</td>
          <td>{c.reason ?? ''}</td>
          <td class="nowrap">{ago(c.created_at_ms)}</td>
        </tr>
      {/each}
    </tbody>
  </table>
</div>
