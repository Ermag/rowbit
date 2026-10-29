<script>
  // One value, shown so it can't pass for another: NULL apart from the
  // text 'NULL', numbers plain, text in quotes, bytes as hex. The server
  // has already made hidden characters visible; this only inserts text.
  let { value } = $props();
</script>

{#if value == null}
  <span class="absent">-</span>
{:else if value.kind === 'null'}
  <i class="null">NULL</i>
{:else if value.kind === 'number'}
  <span class="number">{value.text}</span>
{:else if value.kind === 'binary'}
  <span class="binary">{value.text}</span>
{:else}
  <span class="text">"{value.text}"</span>
{/if}
