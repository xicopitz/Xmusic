# Network mounts (rclone, SMB, NFS)

xmusic is built to be gentle on a library that lives on a network share. On a mount, opening a file costs a
round trip, and after any write rclone uploads the whole file again.

## What xmusic does about it

- **One write per file per run.** Tags, cover and lyrics are gathered per album and written together.
- **No-op writes are skipped.** A file whose values already match is not touched.
- **No re-read after a write.** The database is updated from what was written.
- **Incremental scans.** Only files whose modification time or size changed are read.
- **Bounded parallelism.** File work uses a pool sized by **Parallel file operations** (default 3). Raising it
  mostly adds load, since the limit is the network. Lower it if the host struggles.
- **One MusicBrainz request per second**, never in parallel.

After a run that wrote files, the next scan should read close to zero files. If it re-reads everything, the mount
is not keeping modification times stable.

## rclone mount flags

Scans open thousands of files for a few KB of tags each. Flags worth testing:

```
--vfs-cache-mode writes --buffer-size 0 --vfs-read-ahead 0
```

`--vfs-cache-mode full` downloads every file it opens, and the default buffer reads 16 MB ahead per open.

## Keeping the CPU (and fans) quiet

The compose file caps the container with `cpus: "1.5"`. Fingerprinting runs under `nice -n 19` and looks at
three tracks per album at most.

## Watch mode does not see remote changes

inotify does not work on NFS, SMB or rclone mounts. Use the cron schedule instead.

## File ownership

Run the container as the owner of the files (`user: "1000:1000"` in the compose file) so written tags,
`cover.jpg` and `.lrc` files keep the right owner.
