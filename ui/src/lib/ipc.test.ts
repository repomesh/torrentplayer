// SPDX-License-Identifier: MIT OR Apache-2.0

import { beforeEach, describe, expect, it, vi } from 'vitest';

const invoke = vi.fn();
vi.mock('@tauri-apps/api/core', () => ({ invoke }));

const ipc = await import('./ipc');

beforeEach(() => {
  invoke.mockReset();
});

describe('getAppInfo', () => {
  it('invokes the app_info command with no arguments', async () => {
    const info = { name: 'TorrentPlayer', version: '0.1.0', license: 'MIT OR Apache-2.0' };
    invoke.mockResolvedValue(info);

    await expect(ipc.getAppInfo()).resolves.toEqual(info);
    expect(invoke).toHaveBeenCalledWith('app_info');
  });

  it('propagates backend errors', async () => {
    invoke.mockRejectedValue(new Error('command app_info not allowed'));

    await expect(ipc.getAppInfo()).rejects.toThrow('not allowed');
  });
});

describe('command wrappers', () => {
  it('send camelCase arguments matching the Rust commands', async () => {
    invoke.mockResolvedValue(undefined);
    await ipc.addTorrentLink('magnet:?xt=urn:btih:abc');
    await ipc.startStream(3, 7);
    await ipc.reportPlayback(9, 12.5, 600);
    await ipc.reportPlayback(9, 1, null);
    await ipc.recordPlayerMetric(9, 'seek', 420);
    await ipc.streamStats(9);
    await ipc.streamAudio(9);
    await ipc.streamSubtitles(9);
    await ipc.loadSubtitles(9, 4);
    await ipc.stopStream(9);
    await ipc.devAutoplay();
    await ipc.libraryItems();
    await ipc.previewFile(3, 2);
    await ipc.stopPreview();
    expect(invoke.mock.calls).toEqual([
      ['add_torrent_link', { link: 'magnet:?xt=urn:btih:abc' }],
      ['start_stream', { torrentId: 3, fileIndex: 7 }],
      ['report_playback', { streamId: 9, positionSecs: 12.5, durationSecs: 600 }],
      ['report_playback', { streamId: 9, positionSecs: 1, durationSecs: null }],
      ['record_player_metric', { streamId: 9, kind: 'seek', millis: 420 }],
      ['stream_stats', { streamId: 9 }],
      ['stream_audio', { streamId: 9 }],
      ['stream_subtitles', { streamId: 9 }],
      ['load_subtitles', { streamId: 9, fileIndex: 4 }],
      ['stop_stream', { streamId: 9 }],
      ['dev_autoplay'],
      ['library_items'],
      ['preview_file', { torrentId: 3, fileIndex: 2 }],
      ['stop_preview'],
    ]);
  });

  it('sends .torrent bytes as the raw request body', async () => {
    invoke.mockResolvedValue(undefined);
    const bytes = new Uint8Array([100, 101]);
    await ipc.addTorrentFile(bytes);
    expect(invoke).toHaveBeenCalledWith('add_torrent_file', bytes);
    await ipc.parseSubtitles(bytes);
    expect(invoke).toHaveBeenCalledWith('parse_subtitles', bytes);
  });
});

describe('errorMessage', () => {
  it('prefers the backend message', () => {
    expect(ipc.errorMessage({ kind: 'not-found', message: 'Unknown torrent.' })).toBe(
      'Unknown torrent.',
    );
    expect(ipc.isIpcError({ kind: 'engine' })).toBe(false);
  });

  it('says what network errors mean', () => {
    const blocked = ipc.errorMessage({
      kind: 'kill-switch',
      message: 'Traffic is blocked by the kill switch: the proxy is unreachable.',
    });
    expect(blocked).toMatch(/^Traffic is blocked by the kill switch: the proxy is unreachable\. /);
    expect(blocked).toMatch(/Nothing was sent/);
    expect(ipc.errorMessage({ kind: 'network-unavailable', message: 'Timed out.' })).toMatch(
      /^Timed out\. Check your internet connection/,
    );
  });

  it('falls back for other rejections', () => {
    expect(ipc.errorMessage(new Error('boom'))).toBe('boom');
    expect(ipc.errorMessage('plain')).toBe('plain');
    expect(ipc.errorMessage(42)).toBe('Something went wrong.');
  });
});
