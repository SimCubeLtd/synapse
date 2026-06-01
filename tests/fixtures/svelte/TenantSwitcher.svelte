<script lang="ts">
  import { onMount } from 'svelte';

  interface Tenant {
    id: string;
    name: string;
  }

  let { tenants }: { tenants: Tenant[] } = $props();
  let current = $state<string | null>(null);

  export function selectTenant(id: string): void {
    current = id;
  }

  function formatLabel(t: Tenant): string {
    return `${t.name} (${t.id})`;
  }

  onMount(() => {
    current = tenants[0]?.id ?? null;
  });
</script>

<select bind:value={current}>
  {#each tenants as t}
    <option value={t.id}>{formatLabel(t)}</option>
  {/each}
</select>
