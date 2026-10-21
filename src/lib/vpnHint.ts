// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * The note that recommends a VPN in Direct mode: in Settings → Network, beside the status bar's
 * network indicator, and once per run when seeding starts. Dismissing it anywhere hides it
 * everywhere (`network.vpnHintDismissed`).
 */

import { listen } from '@tauri-apps/api/event';
import type { NetworkModeKind } from './bindings/NetworkModeKind';
import {
  SEEDING_STARTED_EVENT,
  SETTINGS_CHANGED_EVENT,
  getSettings,
  openLink,
  updateSettings,
  type Settings,
} from './ipc';

/** One sentence. It recommends privacy, and says nothing about what is legal. */
export const VPN_HINT =
  'In a swarm every peer and tracker sees your IP address, so a VPN is recommended when downloading or seeding, for example Windscribe through its app (VPN mode) or with a WireGuard configuration (WireGuard mode).';

/** The blog post on VPNs for torrenting. */
export const VPN_POST_URL = 'https://torrentplayer.io/blog/vpn-for-torrenting/';

/** Whether the note applies: Direct mode, and not dismissed. */
export function showVpnHint(mode: NetworkModeKind, dismissed: boolean): boolean {
  return mode === 'direct' && !dismissed;
}

/** Opens the blog post in the system browser. */
export function openVpnPost(): void {
  void openLink(VPN_POST_URL).catch(() => undefined);
}

/** Hides the note for good. */
export function dismissVpnHint(): Promise<void> {
  return updateSettings({ network: { vpnHintDismissed: true } }).then(() => undefined);
}

/** Registers `onEvent` for a backend event; the returned function stops it. */
function follow(event: string, onEvent: (payload: unknown) => void): () => void {
  let stopped = false;
  let unlisten: (() => void) | null = null;
  listen(event, (e) => {
    if (!stopped) {
      onEvent(e.payload);
    }
  })
    .then((stop) => {
      if (stopped) {
        stop();
      } else {
        unlisten = stop;
      }
    })
    .catch(() => undefined);
  return () => {
    stopped = true;
    unlisten?.();
  };
}

/**
 * Calls `onDismissed` with whether the note is dismissed, now and after every settings change,
 * until the returned function is called.
 */
export function followVpnHintDismissed(onDismissed: (dismissed: boolean) => void): () => void {
  let heard = false;
  const stop = follow(SETTINGS_CHANGED_EVENT, (payload) => {
    heard = true;
    onDismissed((payload as Settings).network.vpnHintDismissed);
  });
  getSettings()
    .then((view) => {
      // An event that arrived first is newer.
      if (!heard) {
        onDismissed(view.settings.network.vpnHintDismissed);
      }
    })
    .catch(() => undefined);
  return stop;
}

/** Calls `onStart` each time a torrent starts seeding after playback. */
export function followSeedingStarted(onStart: () => void): () => void {
  return follow(SEEDING_STARTED_EVENT, () => {
    onStart();
  });
}
