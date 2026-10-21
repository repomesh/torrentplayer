// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * The note that recommends a VPN in Direct mode: in Settings → Network, beside the status bar's
 * network indicator, and once per run when seeding starts. Dismissing it anywhere hides it
 * everywhere (`network.vpnHintDismissed`); every place follows the saved settings.
 */

import type { NetworkMode } from './bindings/NetworkMode';
import type { NetworkSettings } from './bindings/NetworkSettings';
import { follow } from './follow';
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
  "In a swarm every peer and tracker sees your IP address, so a VPN is recommended when downloading or seeding: connect your VPN app (for example Windscribe) first, or use the WireGuard mode with your provider's WireGuard configuration.";

/** The blog post on VPNs for torrenting. */
export const VPN_POST_URL = 'https://torrentplayer.io/blog/vpn-for-torrenting/';

/**
 * Whether the note applies: Direct mode, and not dismissed. Not while the notice about the removed
 * VPN adapter mode (`removedMode`, see `src/lib/removedMode.ts`) is shown, which says the same.
 */
export function showVpnHint(mode: NetworkMode, dismissed: boolean, removedMode = false): boolean {
  return mode === 'direct' && !dismissed && !removedMode;
}

/** Opens the blog post in the system browser. */
export function openVpnPost(): void {
  void openLink(VPN_POST_URL).catch(() => undefined);
}

/** Hides the note for good. Rejects if the setting could not be saved. */
export function dismissVpnHint(): Promise<void> {
  return updateSettings({ network: { vpnHintDismissed: true } }).then(() => undefined);
}

/**
 * Calls `onNetwork` with the saved network settings (mode and dismissal), now and after every
 * settings change, until the returned function is called.
 */
export function followVpnHint(onNetwork: (network: NetworkSettings) => void): () => void {
  return follow<Settings>(
    SETTINGS_CHANGED_EVENT,
    (settings) => {
      onNetwork(settings.network);
    },
    () => getSettings().then((view) => view.settings),
  );
}

/** Calls `onStart` when a torrent starts seeding after its playback ended. */
export function followSeedingStarted(onStart: () => void): () => void {
  return follow<null>(SEEDING_STARTED_EVENT, () => {
    onStart();
  });
}
