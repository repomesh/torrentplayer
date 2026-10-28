# Downloads, saved files, and library removal

TorrentPlayer 0.3.2 saves fully downloaded video and audio files automatically. Open **Files**
and use **Copy path** beside Play to copy a completed file's location. Each file becomes
available independently, so one lecture can be saved while the rest of its torrent downloads.

## Find the download folder

Open **Settings → Downloads & storage** to see the current download folder. Click its path to
select it, or press **Copy path**. These are the usual default locations:

| System  | Default download folder                 |
| ------- | --------------------------------------- |
| Linux   | `~/Downloads/TorrentPlayer`             |
| Windows | `%USERPROFILE%\Downloads\TorrentPlayer` |
| macOS   | `~/Downloads/TorrentPlayer`             |

The app uses your system's Downloads folder, which may have been moved or renamed. On systems
without one, it uses `downloads` in the app's local data folder. The location shown in Settings
is authoritative.

**Choose folder…** changes the location for the next start. Existing downloads stay in the old
folder. See [Downloads and storage](configuration.md#downloads-and-storage) for the storage limit
and automatic cleanup settings.

## Find a completed video or audio file

1. Open **Files** and choose the torrent.
2. Wait for the file to finish downloading and saving.
3. Press **Copy path** beside that file's Play button. Paste the location into your file manager
   or another media player.

You can also select the file to see its full **Saved file** path in the preview panel. Clicking
the path selects it for manual copying. Long paths keep their copy button inside the panel.

With the automatic engine, normal media files are saved under:

```text
<download folder>/torrentplayer-swarm/<torrent hash>/files/<original file path>
```

The original filename, format, and bytes are preserved, including a video's existing audio
tracks. Saving the complete file does not transcode it. Files downloaded by the `rqbit` engine
are stored as normal files under `librqbit/`; use **Copy path** for their exact location.

**Copy path** stays hidden while the file is incomplete or being saved. It appears automatically
when the saved file is ready, including on rows you have not selected. If saving fails, select
the file to read its status; the app retries automatically.

## Understand the piece cache and progress bars

`pieces.dat` and `pieces.idx` hold the automatic engine's verified pieces and their index.
TorrentPlayer keeps them for streaming and sharing alongside the completed media files.
Completed media uses additional disk space, and both the saved files and pieces count toward
the storage limit.

The queue and player show green sections where data has downloaded and gaps where it is
missing. The queue's percentage still gives the total downloaded amount. A playable file can
have gaps later in it, so being ready to start playback does not mean the whole file is saved.

## Remove a torrent and its downloads

1. Open **Library** and find the torrent.
2. Press **Remove torrent**.
3. Review the confirmation, then choose **Remove and delete downloads**.

This stops playback of that torrent and removes all its queue entries, library history, saved
files, and cached pieces from the active download folder. Removal applies to the whole torrent
because its files share one piece cache. Files left in an older download folder after changing
the setting remain there.

The queue's **Remove** action unqueues an item. Use the Library action above when you want to
delete its downloaded data too.

If another program has a file open and deletion fails, close that program and try again. The
library entry is kept until the downloaded files have been removed. Deleted downloads must be
downloaded again if you add the torrent later.
