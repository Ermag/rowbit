<script>
  import { onMount } from 'svelte';
  import * as api from './api.js';
  import { ago } from './format.js';

  let setup = $state(null);
  let error = $state('');

  async function load() {
    try {
      setup = await api.setup();
      error = '';
    } catch (e) {
      error = e.message;
    }
  }

  onMount(() => {
    load();
    const timer = setInterval(load, 15000);
    return () => clearInterval(timer);
  });

  function rules(a) {
    const verb = { allow: 'allows', block: 'refuses' }[a.default] ?? 'holds';
    let s = a.rules === 0 ? `${verb} every write` : `${a.rules} rule${a.rules === 1 ? '' : 's'}, else ${verb}`;
    if (a.mode === 'shadow') s += ' (shadow mode: logged, not enforced)';
    if (a.scoped) s += ', scoped';
    return s;
  }
</script>

<h1>Setup</h1>
<p class="muted">
  What this Rowbit guards, and where its agents are connected. Passwords are never shown.
  <code>rowbit status</code> shows the same for every setup on this machine.
</p>
{#if error}<p class="error">{error}</p>{/if}
{#if setup}
  <section>
    <h2>Database</h2>
    {#if setup.database}
      <dl class="facts">
        <dt>Database</dt><dd><code>{setup.database.database}</code></dd>
        <dt>Server</dt><dd><code>{setup.database.host}:{setup.database.port}</code> (MariaDB or MySQL)</dd>
        <dt>Rowbit connects as</dt><dd><code>{setup.database.user}</code></dd>
        <dt>Connection</dt><dd>{setup.database.tls}</dd>
      </dl>
    {:else}
      <p class="muted">Unknown: the database URL isn't available to the review page.</p>
    {/if}
  </section>

  <section>
    <h2>Rowbit</h2>
    <dl class="facts">
      {#if setup.dir}<dt>Files</dt><dd><code>{setup.dir}</code></dd>{/if}
      <dt>Review page</dt><dd>{setup.review_page}</dd>
      <dt>Sign-in</dt><dd>{setup.sign_in}</dd>
      <dt>Agents over HTTP</dt><dd>{setup.mcp ?? 'off (agents start rowbit mcp themselves)'}</dd>
      {#if setup.notify?.length}<dt>Notifications</dt><dd>{setup.notify.join(', ')}</dd>{/if}
      <dt>Held now</dt><dd><a href="#/">{setup.held}</a></dd>
      <dt>Version</dt><dd>{setup.version}</dd>
    </dl>
  </section>

  <section>
    <h2>Agents</h2>
    {#each setup.agents as a (a.id)}
      <h3><code>{a.id}</code></h3>
      {#if a.description}<p class="muted">{a.description}</p>{/if}
      <p>{rules(a)}</p>
      {#if a.connections.length}
        <div class="scroll">
          <table>
            <thead><tr><th>Connected</th><th>Id</th><th>Since</th><th>Last used</th></tr></thead>
            <tbody>
              {#each a.connections as c (c.id)}
                <tr>
                  <td>{c.label}</td>
                  <td><code>{c.id}</code></td>
                  <td class="nowrap">{ago(c.created_ms)}</td>
                  <td class="nowrap">{c.used_ms ? ago(c.used_ms) : 'never'}</td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
        <p class="muted">Sign one out: <code>rowbit agents revoke {a.id} --connection &lt;id&gt;</code></p>
      {:else}
        <p class="muted">Not connected over HTTP. Connect a client from your project: <code>rowbit connect claude-code</code></p>
      {/if}
    {/each}
  </section>
{/if}
