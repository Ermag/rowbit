<script>
  import { onMount } from 'svelte';
  import * as api from './api.js';
  import { ago, count, verbs } from './format.js';
  import State from './State.svelte';

  let groups = $state(null);
  let error = $state('');

  onMount(async () => {
    try {
      const { changes } = await api.changes({ shadowed: 'true' });
      const byRule = new Map();
      for (const c of changes) {
        const key = c.shadowed.by;
        if (!byRule.has(key)) byRule.set(key, []);
        byRule.get(key).push(c);
      }
      groups = [...byRule.entries()].sort((a, b) => b[1].length - a[1].length);
    } catch (e) {
      error = e.message;
    }
  });
</script>

<h1>Shadow mode</h1>
<p class="muted">
  Changes from agents in shadow mode that a rule would have held or blocked. They were applied, and
  can be undone from their page.
</p>
{#if error}<p class="error">{error}</p>{/if}
{#if groups && groups.length === 0}<p class="muted">Nothing yet.</p>{/if}

{#each groups ?? [] as [rule, list] (rule)}
  <h2>{rule} <span class="muted">({count(list.length, 'change')})</span></h2>
  <div class="scroll">
    <table>
      <thead><tr><th>#</th><th>State</th><th>Agent</th><th>What</th><th>Would have</th><th>When</th></tr></thead>
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
            <td>{c.shadowed.would ?? ''}</td>
            <td class="nowrap">{ago(c.created_at_ms)}</td>
          </tr>
        {/each}
      </tbody>
    </table>
  </div>
{/each}
