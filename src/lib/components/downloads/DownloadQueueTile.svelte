<script lang="ts">
  import { downloadState } from '$lib/state/downloadState.svelte';
  import { configState } from '$lib/state/configState.svelte';
  import { i18n } from '$lib/i18n';
  import { formatBytes } from '$lib/utils/formatters';
  import { notify } from '$lib/utils/toast';
  import Button from '$lib/components/ui/Button.svelte';
  import ConfirmDialog from '$lib/components/ui/ConfirmDialog.svelte';
  import IconPause from '~icons/fluent/pause-20-regular';
  import IconPlay from '~icons/fluent/play-20-regular';
  import IconRetry from '~icons/fluent/arrow-counterclockwise-20-regular';
  import IconDismiss from '~icons/fluent/dismiss-20-regular';

  interface Props {
    onshowfailed?: () => void;
  }

  let { onshowfailed }: Props = $props();

  const ratios = { square: '1 / 1', portrait: '4 / 5', landscape: '3 / 2', widescreen: '16 / 9' } as const;
  let ratio = $derived(ratios[configState.settings.grid_aspect_ratio]);

  let stats = $derived(downloadState.queueStats);
  let running = $derived(stats.downloading + stats.queued > 0);
  let cancellable = $derived(stats.downloading + stats.queued + stats.paused > 0);
  let percent = $derived(
    downloadState.activeProgress === null ? null : Math.floor(downloadState.activeProgress * 100)
  );
  let busy = $state(false);
  let confirmCancel = $state(false);

  async function run(action: () => Promise<void>) {
    if (busy) return;
    busy = true;
    try {
      await action();
    } catch (error) {
      notify.error(i18n.t('downloads.action_error'), error);
    } finally {
      busy = false;
    }
  }

  function toggleAll() {
    void run(() => (running ? downloadState.pauseAll() : downloadState.resumeAll()));
  }

  async function cancelAll() {
    await run(() => downloadState.cancelAll());
    confirmCancel = false;
  }
</script>

<article class="grid-tile queue-tile" style:aspect-ratio={ratio} aria-label={i18n.t('downloads.queue_title')}>
  <div class="queue-body">
    <header class="queue-head">
      <h2 class="queue-title">{i18n.t('downloads.queue_title')}</h2>
      {#if percent !== null}
        <span class="queue-percent">{percent}%</span>
      {/if}
    </header>

    {#if stats.totalBytes > 0 || stats.speedBps > 0}
      <p class="queue-bytes">
        {#if stats.totalBytes > 0}{formatBytes(stats.downloadedBytes)} / {formatBytes(stats.totalBytes)}{/if}
        {#if stats.speedBps > 0}<span>{formatBytes(stats.speedBps)}/s</span>{/if}
      </p>
    {/if}

    <ul class="queue-counts">
      {#if stats.downloading > 0}<li>{i18n.t('downloads.queue_downloading', { count: stats.downloading })}</li>{/if}
      {#if stats.queued > 0}<li>{i18n.t('downloads.queue_queued', { count: stats.queued })}</li>{/if}
      {#if stats.paused > 0}<li>{i18n.t('downloads.queue_paused', { count: stats.paused })}</li>{/if}
      {#if stats.failed > 0}
        <li>
          <button type="button" class="queue-failed" onclick={() => onshowfailed?.()}>
            {i18n.t('downloads.queue_failed', { count: stats.failed })}
          </button>
        </li>
      {/if}
    </ul>

    <div class="queue-actions">
      {#if cancellable}
        {@const label = running ? i18n.t('downloads.pause_all') : i18n.t('downloads.resume_all')}
        <Button variant="ghost" class="queue-toggle" disabled={busy} onclick={toggleAll} title={label} aria-label={label}>
          {#if running}<IconPause />{:else}<IconPlay />{/if}
          <span class="queue-toggle-label">{label}</span>
        </Button>
      {/if}
      {#if stats.failed > 0}
        <Button
          variant="ghost"
          class="btn-icon"
          disabled={busy}
          onclick={() => void run(() => downloadState.retryFailed())}
          title={i18n.t('downloads.retry_failed')}
          aria-label={i18n.t('downloads.retry_failed')}
        >
          <IconRetry />
        </Button>
      {/if}
      {#if cancellable}
        <Button
          variant="ghost"
          class="btn-icon"
          disabled={busy}
          onclick={() => (confirmCancel = true)}
          title={i18n.t('downloads.cancel_all')}
          aria-label={i18n.t('downloads.cancel_all')}
        >
          <IconDismiss />
        </Button>
      {/if}
    </div>
  </div>

  {#if percent !== null || stats.downloading > 0}
    <div class="download-progress" class:indeterminate={percent === null}>
      <span style:width={`${percent ?? 0}%`}></span>
    </div>
  {/if}
</article>

<ConfirmDialog
  isOpen={confirmCancel}
  title={i18n.t('downloads.cancel_all_title')}
  description={i18n.t('downloads.cancel_all_desc')}
  confirmLabel={i18n.t('downloads.cancel_all')}
  confirmVariant="danger"
  confirmIcon={IconDismiss}
  loading={busy}
  onconfirm={() => void cancelAll()}
  onclose={() => (confirmCancel = false)}
/>

<style>
  .queue-tile {
    --queue-page-control: var(--control-height);
    container-type: size;
    background: var(--surface-container-low);
  }

  .queue-body {
    position: absolute;
    inset: 0;
    display: flex;
    flex-direction: column;
    gap: calc(6px * var(--grid-scale, 1));
    padding: var(--tile-inset);
    color: var(--text-primary);
  }

  .queue-head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: calc(8px * var(--grid-scale, 1));
  }

  .queue-title {
    margin: 0;
    font-family: var(--font-display);
    font-size: calc(15px * var(--grid-scale, 1));
    font-weight: 600;
    line-height: 1.25;
  }

  .queue-percent {
    font-family: var(--font-mono);
    font-size: calc(22px * var(--grid-scale, 1));
    font-weight: 600;
    font-variant-numeric: tabular-nums;
    line-height: 1;
    color: var(--accent-on-surface, var(--accent-primary));
  }

  .queue-bytes {
    display: flex;
    flex-wrap: wrap;
    gap: 0 calc(8px * var(--grid-scale, 1));
    margin: 0;
    font-family: var(--font-mono);
    font-size: calc(11px * var(--grid-scale, 1));
    font-variant-numeric: tabular-nums;
    color: var(--text-secondary);
  }

  .queue-counts {
    display: flex;
    flex-direction: column;
    gap: calc(2px * var(--grid-scale, 1));
    min-height: 0;
    margin: 0;
    padding: 0;
    overflow: hidden;
    list-style: none;
    font-size: calc(12px * var(--grid-scale, 1));
    line-height: 1.4;
    color: var(--text-secondary);
  }

  .queue-failed {
    padding: 0;
    border: none;
    background: none;
    font: inherit;
    color: var(--status-error);
    text-decoration: underline;
    text-underline-offset: 0.2em;
    cursor: pointer;
  }

  .queue-actions {
    --queue-action-gap: calc(4px * var(--grid-scale, 1));
    /* Page control height, shrunk only when three buttons don't fit the tile. */
    --control-height: min(
      var(--queue-page-control),
      calc((100cqw - 2 * var(--tile-inset) - 2 * var(--queue-action-gap)) / 3 / var(--ui-scale, 1))
    );
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: var(--queue-action-gap);
    margin-top: auto;
  }

  .queue-actions :global(.queue-toggle) {
    flex: 1 1 auto;
    min-width: calc(var(--control-height) * var(--ui-scale, 1));
  }

  .queue-toggle-label {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  @container (max-width: 190px) {
    .queue-percent { font-size: calc(18px * var(--grid-scale, 1)); }
    .queue-toggle-label { display: none; }
  }

  @container (max-height: 150px) {
    .queue-counts { display: none; }
  }
</style>
