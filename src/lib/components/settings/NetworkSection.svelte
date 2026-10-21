<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<script lang="ts">
  /*
   * Connection mode (direct, VPN adapter, SOCKS5, built-in WireGuard), the live
   * connection state from the network layer, the connection check, and WireGuard import.
   * Saving a setting applies it: the backend follows every change, so there is no Apply button.
   */
  import { onMount } from 'svelte';
  import type { NetworkMode } from '../../bindings/NetworkMode';
  import type { WireGuardSettings } from '../../bindings/WireGuardSettings';
  import {
    checkConnection,
    clearSecret,
    errorMessage,
    importWireGuardConfig,
    listInterfaces,
    recheckNetwork,
    validateWireGuardConfig,
    type NetworkInterface,
    type NetworkStatus,
    type PublicIp,
    type SecretStatus,
    type Settings,
    type SettingsView,
    type WireGuardSummary,
  } from '../../ipc';
  import { MODE_NAMES, describeNetwork, followNetwork, interfaceAdvice } from '../../network';
  import Field from './Field.svelte';
  import SecretField from './SecretField.svelte';
  import Toggle from './Toggle.svelte';

  interface Props {
    settings: Settings;
    errors: Record<string, string>;
    update: (path: string, value: unknown) => void;
    secrets: SecretStatus | null;
    onSecrets: (status: SecretStatus) => void;
    onView: (view: SettingsView) => void;
  }

  let { settings, errors, update, secrets, onSecrets, onView }: Props = $props();

  const MODES: { value: NetworkMode; label: string; help: string }[] = [
    {
      value: 'direct',
      label: 'Direct',
      help: 'Connect straight to peers. They can see your IP address.',
    },
    {
      value: 'bindInterface',
      label: 'VPN',
      help: 'Use your connected VPN app, including OpenVPN, Proton VPN, NordVPN, or Mullvad. Choose its tunnel adapter below; traffic pauses if it disconnects.',
    },
    {
      value: 'socks5',
      label: 'SOCKS5 proxy',
      help: 'Route traffic through a SOCKS5 proxy (many VPN providers offer one). If it stops answering, traffic pauses.',
    },
    {
      value: 'wireGuard',
      label: 'WireGuard',
      help: 'Import a WireGuard .conf file from your VPN provider and connect inside TorrentStream. No separate VPN app or administrator rights are needed.',
    },
  ];

  const EMPTY_WIREGUARD: WireGuardSettings = {
    name: null,
    addresses: [],
    dns: [],
    peerPublicKey: null,
    peerEndpoint: null,
    allowedIps: [],
    persistentKeepalive: null,
    mtu: null,
  };

  /** Events listed under the status. */
  const SHOWN_EVENTS = 6;

  const n = $derived(settings.network);
  const wg = $derived(n.wireguard);
  const wgConfigured = $derived(wg.peerEndpoint !== null && wg.peerPublicKey !== null);
  const storeAvailable = $derived(secrets?.storeAvailable ?? true);

  let status = $state<NetworkStatus | null>(null);
  let rechecking = $state(false);
  let interfaces = $state<NetworkInterface[] | null>(null);
  let interfacesError = $state<string | null>(null);
  let checking = $state(false);
  let check = $state<PublicIp | null>(null);
  let checkMessage = $state<string | null>(null);

  // WireGuard import: check first (a summary without keys), then save.
  let importText = $state('');
  let pending = $state<{ text: string; name: string | null; summary: WireGuardSummary } | null>(
    null,
  );
  let importing = $state(false);
  let importMessage = $state<string | null>(null);
  let fileInput: HTMLInputElement | undefined = $state();

  const summary = $derived(status === null ? null : describeNetwork(status, Date.now()));
  const events = $derived(status === null ? [] : status.events.slice(-SHOWN_EVENTS).reverse());
  const canRetry = $derived(
    status !== null && status.mode !== 'direct' && status.state.kind !== 'connected',
  );

  const choices = $derived.by(() => {
    if (interfaces === null) {
      return [];
    }
    const usable = interfaces.filter((i) => !i.isLoopback);
    const current = n.interface;
    if (current !== null && !usable.some((i) => i.name === current)) {
      usable.push({
        name: current,
        addresses: [],
        isUp: false,
        isLoopback: false,
        likelyVpn: false,
      });
    }
    return usable.sort(
      (a, b) =>
        Number(b.likelyVpn) - Number(a.likelyVpn) ||
        Number(b.isUp) - Number(a.isUp) ||
        a.name.localeCompare(b.name),
    );
  });

  function loadInterfaces(): void {
    listInterfaces()
      .then((list) => {
        interfaces = list;
        interfacesError = null;
      })
      .catch((reason: unknown) => {
        interfacesError = errorMessage(reason);
      });
  }

  onMount(() => {
    loadInterfaces();
    return followNetwork((next) => {
      status = next;
    });
  });

  function time(ms: number): string {
    return new Date(ms).toLocaleTimeString(undefined, {
      hour: '2-digit',
      minute: '2-digit',
      second: '2-digit',
    });
  }

  async function retry(): Promise<void> {
    rechecking = true;
    try {
      status = await recheckNetwork();
    } catch (reason: unknown) {
      checkMessage = errorMessage(reason);
    } finally {
      rechecking = false;
    }
  }

  async function runCheck(): Promise<void> {
    checking = true;
    check = null;
    checkMessage = null;
    try {
      check = await checkConnection();
    } catch (reason: unknown) {
      checkMessage = errorMessage(reason);
    } finally {
      checking = false;
    }
  }

  /** Checks a configuration and shows what it contains; nothing is saved yet. */
  async function preview(text: string, name: string | null): Promise<void> {
    if (text.trim().length === 0 || importing) {
      return;
    }
    importing = true;
    importMessage = null;
    pending = null;
    try {
      pending = { text, name, summary: await validateWireGuardConfig(text) };
    } catch (reason: unknown) {
      importMessage = errorMessage(reason);
    } finally {
      importing = false;
    }
  }

  async function save(): Promise<void> {
    if (pending === null || importing) {
      return;
    }
    importing = true;
    importMessage = null;
    try {
      const result = await importWireGuardConfig(pending.text, pending.name);
      onView(result.view);
      onSecrets(result.secrets);
      importText = '';
      pending = null;
      importMessage = 'Configuration saved. Connecting to the WireGuard server…';
    } catch (reason: unknown) {
      importMessage = errorMessage(reason);
    } finally {
      importing = false;
    }
  }

  function cancelImport(): void {
    pending = null;
    importText = '';
    importMessage = null;
  }

  async function importFile(file: File): Promise<void> {
    if (fileInput) {
      fileInput.value = '';
    }
    if (file.size > 64 * 1024) {
      importMessage = 'That file is too large to be a WireGuard configuration.';
      return;
    }
    // The file's text (with its private key) is never put on screen.
    importText = '';
    await preview(await file.text(), file.name);
  }

  async function removeWireGuard(): Promise<void> {
    update('network.wireguard', EMPTY_WIREGUARD);
    try {
      await clearSecret('wireGuardPresharedKey');
      onSecrets(await clearSecret('wireGuardPrivateKey'));
      importMessage = 'Configuration removed.';
    } catch (reason: unknown) {
      importMessage = errorMessage(reason);
    }
  }
</script>

<section class="card" aria-labelledby="network-title">
  <h3 id="network-title">VPN and network</h3>

  {#if status && summary}
    <div class="status {summary.tone}" role="status" aria-live="polite">
      <div class="status-line">
        <span class="dot" aria-hidden="true"></span>
        <strong class="status-label">{summary.label}</strong>
        {#if canRetry}
          <button type="button" disabled={rechecking} onclick={() => void retry()}>
            {rechecking ? 'Checking…' : 'Try again'}
          </button>
        {/if}
      </div>
      <dl class="facts">
        <dt>Mode</dt>
        <dd>{MODE_NAMES[status.mode]}</dd>
        {#if status.endpoint}
          <dt>{status.mode === 'bindInterface' ? 'Interface' : 'Endpoint'}</dt>
          <dd>{status.endpoint}</dd>
        {/if}
        {#if status.bound}
          <dt>In use</dt>
          <dd>{status.bound}</dd>
        {/if}
        <dt>Torrent traffic</dt>
        <dd>
          {status.trafficAllowed ? 'Allowed' : 'Paused by the kill switch'}{status.trafficAllowed &&
          !status.trackersAllowed
            ? ' · trackers off'
            : ''}{status.trafficAllowed && !status.udpAllowed
            ? ' · DHT and UDP off in this mode'
            : ''}{status.trafficAllowed && status.udpAllowed && status.mode === 'bindInterface'
            ? ' · DHT and UDP trackers through the interface'
            : status.trafficAllowed && status.udpAllowed && status.mode === 'wireGuard'
              ? ' · DHT and UDP trackers through WireGuard'
              : status.trafficAllowed && status.mode === 'socks5'
                ? ' · UDP relay when supported by the proxy'
                : ''}
        </dd>
        {#if status.lastCheckMs !== null}
          <dt>Last checked</dt>
          <dd>{time(status.lastCheckMs)}</dd>
        {/if}
      </dl>
      {#if events.length > 0}
        <details class="events">
          <summary>Recent events</summary>
          <ul>
            {#each events as event, i (i)}
              <li class={event.level}>
                <span class="when">{time(event.atMs)}</span>
                {event.message}
              </li>
            {/each}
          </ul>
        </details>
      {/if}
    </div>
  {/if}

  <fieldset class="modes">
    <legend>Connection</legend>
    {#each MODES as mode (mode.value)}
      <label class="mode" class:selected={n.mode === mode.value}>
        <input
          type="radio"
          name="network-mode"
          value={mode.value}
          checked={n.mode === mode.value}
          onchange={() => {
            update('network.mode', mode.value);
          }}
        />
        <span class="mode-text">
          <span class="mode-label">
            {mode.label}
          </span>
          <span class="mode-help">{mode.help}</span>
        </span>
      </label>
    {/each}
  </fieldset>

  {#if n.mode === 'bindInterface'}
    <p class="banner">
      This option uses a VPN app already connected on your computer. For a WireGuard
      <code>.conf</code> file, choose <strong>WireGuard</strong> instead.
    </p>
    {#if interfaces !== null && !interfaces.some((iface) => iface.likelyVpn)}
      <p class="note">No VPN adapter detected. Connect your VPN app, then press Refresh.</p>
    {/if}
    <Field
      id="network-interface"
      label="VPN adapter"
      help="Connect your VPN app first, then select its tunnel adapter. OpenVPN may use a TAP, TUN, or DCO adapter. Refresh after connecting."
      error={errors['network.interface'] ?? interfacesError ?? undefined}
    >
      <div class="inline-controls">
        {#if choices.length > 0}
          <select
            id="network-interface"
            value={n.interface ?? ''}
            aria-describedby="network-interface-help"
            onchange={(e: Event) => {
              const value = (e.currentTarget as HTMLSelectElement).value;
              update('network.interface', value === '' ? null : value);
            }}
          >
            <option value="">Choose…</option>
            {#each choices as iface (iface.name)}
              <option value={iface.name}>
                {iface.name}{iface.likelyVpn ? ' (VPN)' : ''}{iface.isUp ? '' : ' — down'}{iface
                  .addresses.length > 0
                  ? ` · ${iface.addresses.join(', ')}`
                  : ''}
              </option>
            {/each}
          </select>
        {:else}
          <input
            id="network-interface"
            class="wide"
            type="text"
            spellcheck="false"
            autocomplete="off"
            placeholder="wg0"
            value={n.interface ?? ''}
            aria-describedby="network-interface-help"
            aria-invalid={errors['network.interface'] !== undefined}
            onchange={(e: Event) => {
              const value = (e.currentTarget as HTMLInputElement).value.trim();
              update('network.interface', value === '' ? null : value);
            }}
          />
        {/if}
        <button type="button" onclick={loadInterfaces}>Refresh</button>
      </div>
    </Field>
    {#if n.interface && interfaceAdvice(n.interface)}
      <p class="banner" role="status">{interfaceAdvice(n.interface)}</p>
    {/if}
  {/if}

  {#if n.mode === 'socks5'}
    <Field
      id="network-host"
      label="Proxy host"
      help="Host name or IP address, without socks5:// or a port."
      error={errors['network.socks5.host']}
    >
      <input
        id="network-host"
        class="wide"
        type="text"
        spellcheck="false"
        autocomplete="off"
        placeholder="10.64.0.1"
        value={n.socks5.host}
        aria-describedby="network-host-help"
        aria-invalid={errors['network.socks5.host'] !== undefined}
        oninput={(e: Event) => {
          update('network.socks5.host', (e.currentTarget as HTMLInputElement).value.trim());
        }}
      />
    </Field>
    <Field id="network-port" label="Port" error={errors['network.socks5.port']}>
      <input
        id="network-port"
        class="number"
        type="number"
        min="1"
        max="65535"
        step="1"
        value={n.socks5.port}
        aria-invalid={errors['network.socks5.port'] !== undefined}
        oninput={(e: Event) => {
          update('network.socks5.port', (e.currentTarget as HTMLInputElement).valueAsNumber);
        }}
      />
    </Field>
    <Field
      id="network-user"
      label="User name"
      help="Leave empty if the proxy does not need one."
      error={errors['network.socks5.username']}
    >
      <input
        id="network-user"
        type="text"
        spellcheck="false"
        autocomplete="off"
        value={n.socks5.username ?? ''}
        aria-describedby="network-user-help"
        aria-invalid={errors['network.socks5.username'] !== undefined}
        onchange={(e: Event) => {
          const value = (e.currentTarget as HTMLInputElement).value;
          update('network.socks5.username', value === '' ? null : value);
        }}
      />
    </Field>
    <Field
      id="network-password"
      label="Password"
      help={storeAvailable
        ? 'Stored in the system credential store, never in the settings file. With a user name and no password, traffic pauses.'
        : 'No system credential store is available, so passwords cannot be saved. On Linux, install and unlock GNOME Keyring or KWallet.'}
    >
      <SecretField
        id="network-password"
        secret="socks5Password"
        isSet={secrets?.socks5Password ?? false}
        available={storeAvailable}
        onStatus={onSecrets}
      />
    </Field>
    <Field
      id="network-proxy-dns"
      label="Resolve names through the proxy"
      path="network.socks5.proxyDns"
      help="On: the proxy looks up host names, so your DNS server never sees them. Off: TorrentStream looks them up and sends only addresses."
    >
      <Toggle
        id="network-proxy-dns"
        checked={n.socks5.proxyDns}
        describedby="network-proxy-dns-help"
        onchange={(v) => {
          update('network.socks5.proxyDns', v);
        }}
      />
    </Field>
    <Field
      id="network-proxy-trackers"
      label="Send tracker requests through the proxy"
      path="network.socks5.proxyTrackers"
      help="Off: trackers are not contacted at all in this mode (never directly), so fewer peers are found."
    >
      <Toggle
        id="network-proxy-trackers"
        checked={n.socks5.proxyTrackers}
        describedby="network-proxy-trackers-help"
        onchange={(v) => {
          update('network.socks5.proxyTrackers', v);
        }}
      />
    </Field>
    <Field
      id="network-proxy-dht"
      label="DHT through the proxy"
      path="network.socks5.proxyDht"
      help="Find peers through SOCKS5 UDP ASSOCIATE. Requires a proxy with UDP support; never falls back to direct traffic."
    >
      <Toggle
        id="network-proxy-dht"
        checked={n.socks5.proxyDht}
        describedby="network-proxy-dht-help"
        onchange={(v) => {
          update('network.socks5.proxyDht', v);
        }}
      />
    </Field>
  {/if}

  <Field
    id="network-kill"
    label="Kill switch"
    help="Always on when a VPN or proxy mode is selected: if the interface or proxy stops working, torrent traffic pauses and searches are refused, never falling back to a direct connection. Traffic resumes by itself."
  >
    <output id="network-kill" class="fixed" aria-describedby="network-kill-help">
      {n.mode === 'direct' ? 'Not needed in Direct mode' : 'Always on'}
    </output>
  </Field>
  <Field
    id="network-encryption"
    label="Protocol encryption"
    path="network.encryption"
    help="Encrypts BitTorrent traffic between peers. It hides the protocol, not your IP address. Saved now; neither engine applies it yet."
  >
    <select
      id="network-encryption"
      value={n.encryption}
      aria-describedby="network-encryption-help"
      onchange={(e: Event) => {
        update('network.encryption', (e.currentTarget as HTMLSelectElement).value);
      }}
    >
      <option value="prefer">Prefer</option>
      <option value="require">Require</option>
      <option value="off">Off</option>
    </select>
  </Field>
  <Field
    id="network-ipv6"
    label="IPv6"
    path="network.ipv6"
    help="In VPN and proxy modes: Disable refuses IPv6 destinations, so IPv6 cannot leave outside the tunnel."
  >
    <select
      id="network-ipv6"
      value={n.ipv6}
      aria-describedby="network-ipv6-help"
      onchange={(e: Event) => {
        update('network.ipv6', (e.currentTarget as HTMLSelectElement).value);
      }}
    >
      <option value="disable">Disable (IPv4 only)</option>
      <option value="tunnel">Route through the tunnel</option>
    </select>
  </Field>
  <Field
    id="network-show-ip"
    label="Show public IP in the status bar"
    path="network.showPublicIp"
    help="Asks icanhazip.com for your public IPv4 and IPv6 addresses, through the current mode, when the connection changes and every 10 minutes. Never while traffic is paused."
  >
    <Toggle
      id="network-show-ip"
      checked={n.showPublicIp}
      describedby="network-show-ip-help"
      onchange={(v) => {
        update('network.showPublicIp', v);
      }}
    />
  </Field>
  <Field
    id="network-check"
    label="Check connection"
    help="Asks icanhazip.com which IPv4 and IPv6 addresses it sees, through the current mode, now."
  >
    <button id="network-check" type="button" disabled={checking} onclick={() => void runCheck()}>
      {checking ? 'Checking…' : 'Check'}
    </button>
  </Field>
  {#if check || checkMessage}
    <p class="note" role="status">
      {#if check}
        Public IPv4: <strong>{check.ipv4 ?? 'none'}</strong>, IPv6:
        <strong>{check.ipv6 ?? 'none'}</strong>, through {MODE_NAMES[check.mode]}{check.mode ===
        'direct'
          ? ' — these are your own addresses, visible to peers'
          : ''}. Checked with {check.service} at {time(check.checkedAtMs)}.
      {:else}
        {checkMessage}
      {/if}
    </p>
  {/if}
</section>

{#if n.mode === 'wireGuard'}
  <section class="card" aria-labelledby="wireguard-title">
    <h3 id="wireguard-title">WireGuard configuration</h3>
    <p class="intro">
      Import a <code>.conf</code> file from your VPN provider. TorrentStream connects through its
      own WireGuard tunnel, carrying search, trackers, and peer connections. Your keys stay in the
      system credential store. Traffic stays paused until the server authenticates. The
      configuration must include a <code>DNS</code> server for search and other host names.
    </p>
    {#if wgConfigured}
      <dl class="summary">
        {#if wg.name}<dt>Name</dt>
          <dd>{wg.name}</dd>{/if}
        <dt>Endpoint</dt>
        <dd>{wg.peerEndpoint}</dd>
        <dt>Addresses</dt>
        <dd>{wg.addresses.join(', ') || '—'}</dd>
        <dt>Routed</dt>
        <dd>{wg.allowedIps.join(', ') || '—'}</dd>
        <dt>Server key</dt>
        <dd class="key">{wg.peerPublicKey}</dd>
        <dt>Private key</dt>
        <dd>{secrets?.wireGuardPrivateKey ? '••••••• stored' : 'Not stored'}</dd>
      </dl>
      <button
        type="button"
        disabled={rechecking || !secrets?.wireGuardPrivateKey}
        onclick={() => void retry()}
      >
        {rechecking
          ? 'Connecting…'
          : status?.state.kind === 'connected'
            ? 'Check tunnel'
            : 'Connect'}
      </button>
      <button type="button" class="danger-quiet" onclick={() => void removeWireGuard()}>
        Remove configuration
      </button>
    {/if}

    {#if pending}
      {@const s = pending.summary}
      <div class="preview" role="status">
        <h4>{pending.name ?? 'Pasted configuration'}: valid</h4>
        <dl class="summary">
          <dt>Addresses</dt>
          <dd>{s.addresses.join(', ') || '—'}</dd>
          {#if s.dns.length > 0}
            <dt>DNS</dt>
            <dd>{s.dns.join(', ')}</dd>
          {/if}
          {#each s.peers as peer (peer.publicKey)}
            <dt>Server</dt>
            <dd>
              {peer.endpoint}{peer.hasPresharedKey ? ' · with a preshared key' : ''}
            </dd>
            <dt>Routed</dt>
            <dd>{peer.allowedIps.join(', ')}</dd>
          {/each}
          <dt>Keys</dt>
          <dd>Not shown. Saving stores them in the system credential store.</dd>
        </dl>
        {#if s.dns.length === 0}
          <p class="note">
            No DNS server is configured. Add your provider's DNS address to the configuration to use
            search and tracker host names.
          </p>
        {/if}
        {#if !s.routesAllTraffic}
          <p class="note">This configuration does not route all traffic through the tunnel.</p>
        {/if}
        {#each s.warnings as warning, i (i)}
          <p class="note">{warning}</p>
        {/each}
        <div class="actions">
          <button
            type="button"
            class="primary"
            disabled={importing || !storeAvailable}
            onclick={() => void save()}>{importing ? 'Saving…' : 'Save configuration'}</button
          >
          <button type="button" disabled={importing} onclick={cancelImport}>Cancel</button>
        </div>
        {#if !storeAvailable}
          <p class="note">No system credential store is available, so the keys cannot be saved.</p>
        {/if}
      </div>
    {:else}
      <div class="import">
        <label for="wireguard-text">Paste a configuration</label>
        <textarea
          id="wireguard-text"
          rows="5"
          spellcheck="false"
          autocomplete="off"
          placeholder="[Interface]&#10;PrivateKey = …&#10;Address = …&#10;&#10;[Peer]&#10;PublicKey = …&#10;Endpoint = …"
          bind:value={importText}></textarea>
        <div class="actions">
          <button
            type="button"
            class="primary"
            disabled={importing || importText.trim().length === 0}
            onclick={() => void preview(importText, null)}
            >{importing ? 'Checking…' : 'Check configuration'}</button
          >
          <button type="button" disabled={importing} onclick={() => fileInput?.click()}>
            Choose .conf file…
          </button>
          <input
            bind:this={fileInput}
            type="file"
            accept=".conf,text/plain"
            hidden
            onchange={(e: Event) => {
              const file = (e.currentTarget as HTMLInputElement).files?.[0];
              if (file) {
                void importFile(file);
              }
            }}
          />
        </div>
      </div>
    {/if}
    {#if importMessage}
      <p class="note" role="status">{importMessage}</p>
    {/if}
  </section>
{/if}

<style>
  .banner {
    margin: 0 0 1rem;
    padding: 0.75rem 1rem;
    border-radius: 0.6rem;
    background: rgb(242 180 90 / 8%);
    color: var(--text);
    font-size: 0.85rem;
    line-height: 1.45;
  }

  .status {
    margin: 0 0 1.25rem;
    padding: 0.85rem 1rem;
    border: 1px solid rgb(255 255 255 / 7%);
    border-radius: 0.6rem;
    background: rgb(0 0 0 / 18%);
    font-size: 0.85rem;
  }

  .status-line {
    display: flex;
    flex-wrap: wrap;
    gap: 0.5rem;
    align-items: center;
  }

  .status-label {
    flex: 1;
    min-width: 0;
    font-weight: 500;
  }

  .dot {
    flex: none;
    width: 0.55rem;
    height: 0.55rem;
    border-radius: 50%;
    background: #6b6862;
  }

  .protected .dot {
    background: #57b26f;
  }

  .pending .dot {
    background: #d9b44a;
  }

  .blocked .dot {
    background: #d9604f;
  }

  .blocked .status-label {
    color: #e8907a;
  }

  .facts {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 0.25rem 1rem;
    margin: 0.65rem 0 0;
  }

  .events {
    margin-top: 0.6rem;
  }

  .events summary {
    color: var(--text-muted);
    cursor: pointer;
  }

  .events ul {
    margin: 0.4rem 0 0;
    padding: 0;
    list-style: none;
  }

  .events li {
    padding: 0.15rem 0;
    line-height: 1.4;
  }

  .events li.warn {
    color: #e3c27a;
  }

  .events li.error {
    color: #e8907a;
  }

  .when {
    margin-right: 0.5rem;
    color: var(--text-muted);
    font-variant-numeric: tabular-nums;
  }

  .modes {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(14rem, 1fr));
    gap: 0.6rem;
    margin: 0 0 0.75rem;
    padding: 0;
    border: none;
  }

  legend {
    margin-bottom: 0.6rem;
    padding: 0;
    color: var(--text-muted);
    font-size: 0.8rem;
    letter-spacing: 0.06em;
    text-transform: uppercase;
  }

  .mode {
    display: flex;
    gap: 0.6rem;
    align-items: flex-start;
    padding: 0.75rem 0.9rem;
    border: 1px solid rgb(255 255 255 / 8%);
    border-radius: 0.6rem;
    cursor: pointer;
  }

  .mode.selected {
    border-color: rgb(242 180 90 / 55%);
    background: rgb(242 180 90 / 6%);
  }

  .mode:has(input:focus-visible) {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }

  .mode input {
    margin: 0.2rem 0 0;
    accent-color: var(--accent);
  }

  .mode-text {
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
  }

  .mode-help {
    color: var(--text-muted);
    font-size: 0.8rem;
    line-height: 1.4;
  }

  .inline-controls {
    display: flex;
    gap: 0.5rem;
    align-items: center;
  }

  .fixed {
    color: var(--text-muted);
    font-size: 0.85rem;
    white-space: nowrap;
  }

  .intro {
    margin: 0 0 1rem;
    color: var(--text-muted);
    font-size: 0.85rem;
    line-height: 1.45;
  }

  .summary {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 0.35rem 1rem;
    margin: 0 0 0.75rem;
    font-size: 0.85rem;
  }

  dt {
    color: var(--text-muted);
  }

  dd {
    margin: 0;
    word-break: break-all;
  }

  .key {
    font-family: ui-monospace, 'Cascadia Mono', Consolas, monospace;
    font-size: 0.8rem;
  }

  .preview {
    margin-top: 0.75rem;
    padding: 0.85rem 1rem;
    border: 1px solid rgb(242 180 90 / 30%);
    border-radius: 0.6rem;
  }

  .import {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
    margin-top: 0.75rem;
  }

  textarea {
    box-sizing: border-box;
    width: 100%;
    padding: 0.6rem 0.75rem;
    border: 1px solid rgb(255 255 255 / 10%);
    border-radius: 0.5rem;
    background: rgb(0 0 0 / 25%);
    color: var(--text);
    font-family: ui-monospace, 'Cascadia Mono', Consolas, monospace;
    font-size: 0.8rem;
    resize: vertical;
  }

  textarea:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }

  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: 0.5rem;
  }
</style>
