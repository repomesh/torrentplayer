// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * The notice shown while `network.hold` is set: the backend loaded settings without knowing the
 * connection the user chose, so while the mode is Direct it holds all traffic. That happens
 * after an update from the VPN adapter mode (removed in 0.3.0), and when `settings.json` (or its
 * network settings) could not be read. The notice shows in Settings → Network & VPN and beside
 * the status bar's network indicator until it is dismissed, which clears the hold and lets
 * Direct connect.
 */

import type { NetworkHold } from './bindings/NetworkHold';
import type { NetworkSettings } from './bindings/NetworkSettings';
import { updateSettings } from './ipc';

/** What changed and what to do instead, after an update from the VPN adapter mode. */
export const REMOVED_MODE_NOTICE =
  "The VPN adapter mode was removed in 0.3.0. TorrentPlayer now connects directly — keep your VPN app connected, or use the built-in WireGuard mode with your provider's WireGuard configuration to get a kill switch.";

/** Added while traffic is held (Direct mode) after an update from the VPN adapter mode. */
export const REMOVED_MODE_HELD =
  'Torrent traffic and searches stay blocked until you dismiss this note.';

/** The settings could not be read, and traffic is held (Direct mode). */
export const UNREADABLE_NOTICE =
  'Your network settings could not be read, so TorrentPlayer is not connecting. Check Settings → Network & VPN: choose your VPN or proxy again, or connect directly.';

/** The settings could not be read, and another mode has been chosen since. */
export const UNREADABLE_CHOSEN =
  'Your network settings could not be read when TorrentPlayer started. Check that the connection in Settings → Network & VPN is the one you want.';

/** Added while traffic is held (Direct mode) after the settings could not be read. */
export const UNREADABLE_HELD =
  'Torrent traffic and searches stay blocked until you choose Connect directly, or pick a VPN or proxy in Network settings.';

/** The status bar button that opens the notice. */
export const HOLD_BUTTON: Record<NetworkHold, string> = {
  vpnAdapterModeRemoved: 'VPN adapter mode removed',
  settingsUnreadable: 'Network settings unreadable',
};

/** The notice's text, and the line added while traffic is held (null when it is not). */
export function holdNotice(
  reason: NetworkHold,
  held: boolean,
): { text: string; held: string | null } {
  if (reason === 'vpnAdapterModeRemoved') {
    return { text: REMOVED_MODE_NOTICE, held: held ? REMOVED_MODE_HELD : null };
  }
  return held
    ? { text: UNREADABLE_NOTICE, held: UNREADABLE_HELD }
    : { text: UNREADABLE_CHOSEN, held: null };
}

/** Whether the notice is shown. */
export function showHoldNotice(network: Pick<NetworkSettings, 'hold'>): boolean {
  return network.hold !== null;
}

/** Whether the backend holds traffic because of it: the notice is pending in Direct mode. */
export function holdApplies(network: Pick<NetworkSettings, 'mode' | 'hold'>): boolean {
  return network.hold !== null && network.mode === 'direct';
}

/** Hides the notice for good (and, in Direct mode, lets traffic flow). Rejects if not saved. */
export function dismissHoldNotice(): Promise<void> {
  return updateSettings({ network: { hold: null } }).then(() => undefined);
}
