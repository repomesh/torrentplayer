# Introduction

This guide is for people who work on TorrentPlayer: how it is designed, why, and how to build, test,
benchmark, and release it. If you only want to use TorrentPlayer, start with the
[README](https://github.com/torrentplayer/torrentplayer#readme). Questions about anything here
are welcome on the [TorrentPlayer Discord server](https://discord.gg/SNsKPAu7pT).

TorrentPlayer streams any torrent, video or audio, straight from BitTorrent ("stream at the speed
of the swarm"). Its top priority is that playback starts fast, seeking feels near-instant, and
buffering stays ahead of the playhead. Most design
decisions in this guide follow from that goal, and from treating every byte that arrives from the
network as untrusted.

Pages describing subsystems that are not built yet are marked **Planned**. They record the
intended design so that work can be reviewed against it; they are updated as each phase lands.
