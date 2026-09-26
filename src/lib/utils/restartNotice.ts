import { listen } from '@tauri-apps/api/event';
import { i18n } from '$lib/i18n';
import { apiRestartApp, type PendingRestart } from '$lib/utils/ipc';
import { notify } from '$lib/utils/toast';

const TOAST_ID = 'restart-pending';

const settingLabels: Record<PendingRestart['settings'][number], string> = {
  webview_proxy: 'settings.webview_proxy_title',
  browser_user_agent: 'settings.network_browser_user_agent',
  transparent_window: 'settings.linux_transparent_window'
};

export function initRestartNotice(): () => void {
  let shownFor = '';

  const unlisten = listen<PendingRestart>('restart-pending', ({ payload }) => {
    const signature = [...payload.settings].sort().join(',');
    if (!signature) {
      if (shownFor) notify.dismiss(TOAST_ID);
      shownFor = '';
      return;
    }
    if (signature === shownFor) return;
    shownFor = signature;

    const settings = payload.settings.map((id) => i18n.t(settingLabels[id])).join(', ');
    notify.warning(i18n.t('settings.restart_pending_title'), {
      id: TOAST_ID,
      duration: Number.POSITIVE_INFINITY,
      description: i18n.t(
        payload.can_restart ? 'settings.restart_pending_desc' : 'settings.restart_pending_reopen_desc',
        { settings }
      ),
      action: payload.can_restart
        ? {
            label: i18n.t('settings.restart_now'),
            onclick: () => {
              apiRestartApp().catch((error) => notify.error(i18n.t('settings.restart_failed'), error));
            }
          }
        : undefined
    });
  });

  return () => {
    void unlisten.then((stop) => stop());
  };
}
